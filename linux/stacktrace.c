#define UNW_LOCAL_ONLY
#include <libunwind.h>

#include <dlfcn.h>
#include <pthread.h>
#include <signal.h>
#include <string.h>
#include <unistd.h>

#include <linux/sched.h>

#include "util/printbuf.h"
#include "util/util.h"

/*
 * Userspace shim for stack_trace_save_tsk(): unwind another thread of
 * the same process via libunwind.
 *
 * The unwind has to run in the target thread's own context, which means
 * a signal handler. We use SIGRTMIN: real-time, designed for app use,
 * less likely to collide with anything else than SIGUSR{1,2}.
 *
 * Concurrency: a single in-flight slot, serialized by bt_lock. Debug
 * paths only — there's never enough contention to make this hot.
 */

#define BT_SIGNAL	SIGRTMIN

struct bt_request {
	unsigned long	*store;
	unsigned int	size;
	unsigned int	skipnr;
	unsigned int	nr_captured;
	pthread_t	target;
	volatile int	done;
};

static pthread_mutex_t bt_lock = PTHREAD_MUTEX_INITIALIZER;
static struct bt_request *volatile bt_in_flight;

static void bt_signal_handler(int sig)
{
	struct bt_request *r = bt_in_flight;
	if (!r || !pthread_equal(pthread_self(), r->target))
		return;

	unw_context_t uc;
	unw_cursor_t cursor;
	unw_word_t ip;
	unsigned int n = 0;

	if (unw_getcontext(&uc) != 0)
		goto done;
	if (unw_init_local(&cursor, &uc) != 0)
		goto done;

	/*
	 * Skip frames the caller doesn't care about (the signal-handler
	 * frame itself, plus whatever skipnr asks for).
	 */
	unsigned int skip = r->skipnr + 1;
	while (skip-- && unw_step(&cursor) > 0)
		;

	/*
	 * Capture-then-step: after init_local + skips, the cursor points at
	 * the topmost frame the caller wanted. That frame's IP is _THIS_IP_
	 * for the original bch2_save_backtrace() call site; stepping before
	 * reading would drop it.
	 */
	do {
		if (unw_get_reg(&cursor, UNW_REG_IP, &ip) < 0)
			break;
		r->store[n++] = (unsigned long)ip;
	} while (n < r->size && unw_step(&cursor) > 0);

done:
	r->nr_captured = n;
	__atomic_store_n(&r->done, 1, __ATOMIC_RELEASE);
}

static void bt_install_handler(void)
{
	struct sigaction sa = { 0 };
	sa.sa_handler = bt_signal_handler;
	sa.sa_flags   = SA_RESTART;
	sigemptyset(&sa.sa_mask);
	sigaction(BT_SIGNAL, &sa, NULL);
}

static void bt_install_handler_once(void)
{
	static pthread_once_t once = PTHREAD_ONCE_INIT;
	pthread_once(&once, bt_install_handler);
}

unsigned int stack_trace_save_tsk(struct task_struct *task,
				  unsigned long *store,
				  unsigned int size,
				  unsigned int skipnr)
{
	if (!task || !task->thread || !size)
		return 0;

	bt_install_handler_once();

	if (pthread_equal(pthread_self(), task->thread)) {
		/* Self-unwind: no signal needed, just walk our own stack. */
		unw_context_t uc;
		unw_cursor_t cursor;
		unw_word_t ip;
		unsigned int n = 0;

		if (unw_getcontext(&uc) != 0)
			return 0;
		if (unw_init_local(&cursor, &uc) != 0)
			return 0;

		unsigned int skip = skipnr + 1;
		while (skip-- && unw_step(&cursor) > 0)
			;

		do {
			if (unw_get_reg(&cursor, UNW_REG_IP, &ip) < 0)
				break;
			store[n++] = (unsigned long)ip;
		} while (n < size && unw_step(&cursor) > 0);
		return n;
	}

	pthread_mutex_lock(&bt_lock);

	struct bt_request r = {
		.store	= store,
		.size	= size,
		.skipnr	= skipnr,
		.target	= task->thread,
	};
	bt_in_flight = &r;

	if (pthread_kill(task->thread, BT_SIGNAL) == 0) {
		while (!__atomic_load_n(&r.done, __ATOMIC_ACQUIRE))
			sched_yield();
	}

	bt_in_flight = NULL;
	pthread_mutex_unlock(&bt_lock);

	return r.nr_captured;
}

/*
 * Userspace shim for %pB, for util/backtrace.rs: an address's symbol, its
 * name as the binary has it - util/backtrace.rs demangles it, for the kernel's
 * frames too.
 *
 * The name and offset from libunwind, which reads .symtab when it's there, so
 * static functions resolve in a binary that isn't stripped. File and line from
 * libdw, dlopened on first use: best effort - an initramfs without libdw.so.1,
 * or without debuginfo, gets name+offset.
 */

struct Dwfl;
struct Dwfl_Module;
struct Dwfl_Line;

struct dwfl_callbacks {
	int  (*find_elf)(struct Dwfl_Module *, void **, const char *,
			 unsigned long, char **, void **);
	int  (*find_debuginfo)(struct Dwfl_Module *, void **, const char *,
			       unsigned long, const char *, const char *,
			       unsigned long, char **);
	int  (*section_address)(struct Dwfl_Module *, void **, const char *,
				unsigned long, const char *, unsigned long,
				const void *, unsigned long *);
	char **debuginfo_path;
};

static struct {
	bool			initialized;
	struct Dwfl		*dwfl;
	struct Dwfl_Module	*(*addrmodule)(struct Dwfl *, unsigned long);
	struct Dwfl_Line	*(*module_getsrc)(struct Dwfl_Module *, unsigned long);
	const char		*(*lineinfo)(struct Dwfl_Line *, unsigned long *,
					     int *, int *, void *, void *);
} dw;

static pthread_once_t dw_once = PTHREAD_ONCE_INIT;

static void dw_init(void)
{
	void *h = dlopen("libdw.so.1", RTLD_LAZY);
	if (!h)
		return;

	struct Dwfl		*(*dwfl_begin)(const struct dwfl_callbacks *);
	int			(*dwfl_linux_proc_report)(struct Dwfl *, pid_t);
	int			(*dwfl_report_end)(struct Dwfl *,
				int (*)(struct Dwfl_Module *, void *,
					const char *, unsigned long, void *),
				void *);
	int			(*find_elf)();
	int			(*find_debuginfo)();

	dwfl_begin		= dlsym(h, "dwfl_begin");
	dwfl_linux_proc_report	= dlsym(h, "dwfl_linux_proc_report");
	dwfl_report_end		= dlsym(h, "dwfl_report_end");
	find_elf		= dlsym(h, "dwfl_linux_proc_find_elf");
	find_debuginfo		= dlsym(h, "dwfl_standard_find_debuginfo");
	dw.addrmodule		= dlsym(h, "dwfl_addrmodule");
	dw.module_getsrc	= dlsym(h, "dwfl_module_getsrc");
	dw.lineinfo		= dlsym(h, "dwfl_lineinfo");

	if (!dwfl_begin || !dwfl_linux_proc_report || !dwfl_report_end ||
	    !find_elf || !find_debuginfo ||
	    !dw.addrmodule || !dw.module_getsrc || !dw.lineinfo)
		return;

	static struct dwfl_callbacks cb;
	cb.find_elf		= (void *) find_elf;
	cb.find_debuginfo	= (void *) find_debuginfo;

	dw.dwfl = dwfl_begin(&cb);
	if (!dw.dwfl)
		return;

	if (dwfl_linux_proc_report(dw.dwfl, getpid()) ||
	    dwfl_report_end(dw.dwfl, NULL, NULL))
		return;

	dw.initialized = true;
}

static const char *dw_addr_to_src(unsigned long addr, int *lineno)
{
	pthread_once(&dw_once, dw_init);
	if (!dw.initialized)
		return NULL;

	struct Dwfl_Module *mod = dw.addrmodule(dw.dwfl, addr);
	if (!mod)
		return NULL;
	struct Dwfl_Line *line = dw.module_getsrc(mod, addr);
	if (!line)
		return NULL;
	return dw.lineinfo(line, NULL, lineno, NULL, NULL, NULL);
}

void bch2_prt_symbol(struct printbuf *out, unsigned long addr)
{
#if UNW_VERSION_CODE(UNW_VERSION_MAJOR, UNW_VERSION_MINOR) >= UNW_VERSION_CODE(1, 7)
	char name[512];
	unw_word_t off;

	if (!unw_get_proc_name_by_ip(unw_local_addr_space, addr,
				     name, sizeof(name), &off, NULL))
		prt_printf(out, "%s+0x%lx", name, (unsigned long) off);
	else
		prt_printf(out, "0x%lx", addr);
#else
	/* libunwind < 1.7 (e.g. Ubuntu 24.04) has no unw_get_proc_name_by_ip() */
	Dl_info info;

	if (dladdr((void *) addr, &info) && info.dli_sname)
		prt_printf(out, "%s+0x%lx", info.dli_sname,
			   addr - (unsigned long) info.dli_saddr);
	else
		prt_printf(out, "0x%lx", addr);
#endif

	/*
	 * A frame's address is where the call returns to, which can be the next
	 * line, or no line at all: the call is the byte before, as %pB has it
	 */
	int lineno = 0;
	const char *src = dw_addr_to_src(addr - 1, &lineno);
	if (src)
		prt_printf(out, " %s:%d", src, lineno);
}
