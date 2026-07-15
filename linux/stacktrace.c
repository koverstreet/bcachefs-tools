#define UNW_LOCAL_ONLY
#include <libunwind.h>

#include <pthread.h>
#include <signal.h>
#include <string.h>

#include <linux/sched.h>

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
