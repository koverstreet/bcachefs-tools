// SPDX-License-Identifier: GPL-2.0
/*
 * random utility code, for bcache but in theory not specific to bcache
 *
 * Copyright 2010, 2011 Kent Overstreet <kent.overstreet@gmail.com>
 * Copyright 2012 Google, Inc.
 */

#include <linux/bio.h>
#include <linux/blkdev.h>
#include <linux/ctype.h>
#include <linux/debugfs.h>
#include <linux/delay.h>
#include <linux/kthread.h>
#include <linux/math64.h>
#include <linux/percpu.h>
#include <linux/preempt.h>
#include <linux/random.h>
#include <linux/rcupdate.h>
#include <linux/rhashtable.h>
#include <linux/string.h>
#include <linux/types.h>
#include <linux/sched/clock.h>
#include <linux/sched/debug.h>
#include <linux/sched/signal.h>
#include <linux/static_key.h>
#include <linux/version.h>

#include "util.h"
#include "util/jump_label_gen.h"
#include "util/percpu_gen.h"

static const char si_units[] = "?kMGTPEZY";

/* string_get_size units: */
static const char *const units_2[] = {
	"B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB", "ZiB", "YiB"
};
static const char *const units_10[] = {
	"B", "kB", "MB", "GB", "TB", "PB", "EB", "ZB", "YB"
};

static int parse_u64(const char *cp, u64 *res)
{
	const char *start = cp;
	u64 v = 0;

	if (!isdigit(*cp))
		return -EINVAL;

	do {
		if (v > U64_MAX / 10)
			return -ERANGE;
		v *= 10;
		if (v > U64_MAX - (*cp - '0'))
			return -ERANGE;
		v += *cp - '0';
		cp++;
	} while (isdigit(*cp));

	*res = v;
	return cp - start;
}

static int bch2_pow(u64 n, u64 p, u64 *res)
{
	*res = 1;

	while (p--) {
		if (*res > div64_u64(U64_MAX, n))
			return -ERANGE;
		*res *= n;
	}
	return 0;
}

static int parse_unit_suffix(const char *cp, u64 *res)
{
	unsigned adv = *cp == ' ';
	cp += adv;

	for (unsigned u = 1; u < strlen(si_units); u++)
		if (*cp == si_units[u]) {
			try(bch2_pow(1024, u, res));
			return adv + 1;
		}

	for (unsigned u = 0; u < ARRAY_SIZE(units_2); u++)
		if (!strncmp(cp, units_2[u], strlen(units_2[u]))) {
			try(bch2_pow(1024, u, res));
			return adv + strlen(units_2[u]);
		}

	for (unsigned u = 0; u < ARRAY_SIZE(units_10); u++)
		if (!strncmp(cp, units_10[u], strlen(units_10[u]))) {
			try(bch2_pow(1000, u, res));
			return adv + strlen(units_10[u]);
		}

	*res = 1;
	return 0;
}


#define parse_or_ret(cp, _f)			\
do {						\
	int _ret = _f;				\
	if (_ret < 0)				\
		return _ret;			\
	cp += _ret;				\
} while (0)

static int __bch2_strtou64_h(const char *cp, u64 *res)
{
	const char *start = cp;
	u64 v = 0, b, f_n = 0, f_d = 1;
	int ret;

	parse_or_ret(cp, parse_u64(cp, &v));

	if (*cp == '.') {
		cp++;
		ret = parse_u64(cp, &f_n);
		if (ret < 0)
			return ret;
		cp += ret;

		try(bch2_pow(10, ret, &f_d));
	}

	parse_or_ret(cp, parse_unit_suffix(cp, &b));

	if (v > div64_u64(U64_MAX, b))
		return -ERANGE;
	v *= b;

	if (f_n > div64_u64(U64_MAX, b))
		return -ERANGE;

	f_n = div64_u64(f_n * b, f_d);
	if (v + f_n < v)
		return -ERANGE;
	v += f_n;

	*res = v;
	return cp - start;
}

static int __bch2_strtoh(const char *cp, u64 *res,
			 u64 t_max, bool t_signed)
{
	bool positive = *cp != '-';
	u64 v = 0;

	if (*cp == '+' || *cp == '-')
		cp++;

	parse_or_ret(cp, __bch2_strtou64_h(cp, &v));

	if (*cp == '\n')
		cp++;
	if (*cp)
		return -EINVAL;

	if (positive) {
		if (v > t_max)
			return -ERANGE;
	} else {
		if (v && !t_signed)
			return -ERANGE;

		if (v > t_max + 1)
			return -ERANGE;
		v = -v;
	}

	*res = v;
	return 0;
}

#define STRTO_H(name, type)					\
int bch2_ ## name ## _h(const char *cp, type *res)		\
{								\
	u64 v = 0;						\
	int ret = __bch2_strtoh(cp, &v, ANYSINT_MAX(type),	\
			ANYSINT_MAX(type) != ((type) ~0ULL));	\
	*res = v;						\
	return ret;						\
}

STRTO_H(strtoull, unsigned long long)
STRTO_H(strtou64, u64)

bool bch2_is_zero(const void *_p, size_t n)
{
	const char *p = _p;
	size_t i;

	for (i = 0; i < n; i++)
		if (p[i])
			return false;
	return true;
}

void bch2_prt_u64_base2_nbits(struct printbuf *out, u64 v, unsigned nr_bits)
{
	while (nr_bits)
		prt_char(out, '0' + ((v >> --nr_bits) & 1));
}

int bch2_save_backtrace(bch_stacktrace *stack, struct task_struct *task, unsigned skipnr,
			gfp_t gfp)
{
#ifdef CONFIG_STACKTRACE
	unsigned nr_entries = 0;
	int ret = 0;

	stack->nr = 0;
	try(darray_make_room_gfp(stack, 32, gfp));

	skipnr += task == current;

	do {
		nr_entries = stack_trace_save_tsk(task, stack->data, stack->size, skipnr);
	} while (nr_entries == stack->size &&
		 !(ret = darray_make_room_gfp(stack, stack->size * 2, gfp)));

	stack->nr = nr_entries;
	return ret;
#else
	return 0;
#endif
}

int bch2_prt_task_backtrace(struct printbuf *out, struct task_struct *task, unsigned skipnr, gfp_t gfp)
{
	skipnr += task == current;

	CLASS(bch_stacktrace, stack)();
	int ret = bch2_save_backtrace(&stack, task, skipnr, gfp);

	bch2_prt_backtrace(out, &stack);
	return ret;
}

/* misc: */

void bch2_bio_map(struct bio *bio, void *base, size_t size)
{
	if (is_vmalloc_addr(base))
		bio_add_vmalloc(bio, base, size);
	else
		bio_add_virt_nofail(bio, base, size);
}

static struct bio *__bio_chain_endio(struct bio *bio)
{
	struct bio *parent = bio->bi_private;

	if (bio->bi_status && !parent->bi_status)
		parent->bi_status = bio->bi_status;
	bio_put(bio);
	return parent;
}

static void bio_chain_endio(struct bio *bio)
{
	BUG();
}

void bio_chain(struct bio *bio, struct bio *parent)
{
	BUG_ON(bio->bi_private || bio->bi_end_io);

	bio->bi_private = parent;
	bio->bi_end_io	= bio_chain_endio;
	bio_inc_remaining(parent);
}

/*
 * Allocate a bio (possibly a chain of bios via bio_chain()) covering
 * [base, base+size) on @bdev starting at sector @sector. Each bio in the
 * chain is capped at BIO_MAX_VECS bvecs; physically contiguous buffers fit
 * in one bvec and the chain typically has length 1.
 *
 * The returned bio is the tail of the chain (the last chunk in sector
 * order), left unsubmitted for the caller to configure (bi_end_io,
 * bi_private, any wrapper state) and submit. Earlier chunks are chained
 * to the bio that immediately follows them and submitted here in sector
 * order, so the block layer sees requests in forward order; callers
 * should wrap their submission in blk_start_plug()/blk_finish_plug() to
 * give the driver a chance to merge.
 *
 * When the caller's tail bio completes, the chain's bi_status reflects
 * any error propagated from the earlier chunks.
 *
 * REQ_PREFLUSH, if set in @opf, applies only to the first (sector-lowest)
 * bio in the chain.
 */
struct bio *bch2_bio_map_and_chain(struct block_device *bdev,
				   void *base, size_t size,
				   sector_t sector,
				   blk_opf_t opf, gfp_t gfp,
				   struct bio_set *bs)
{
	struct bio *tail	= NULL;
	unsigned nr_bvecs	= buf_nr_bvecs(base, size);

	while (size) {
		size_t chunk_size = size;
		unsigned chunk_bvecs = nr_bvecs;

		if (chunk_bvecs > BIO_MAX_VECS) {
			unsigned offset = (unsigned long) base & (PAGE_SIZE - 1);

			chunk_size = (size_t)BIO_MAX_VECS * PAGE_SIZE - offset;
			chunk_bvecs = BIO_MAX_VECS;
			BUG_ON(chunk_size > size);
		}

		struct bio *bio = bio_alloc_bioset(bdev, chunk_bvecs,
						   opf, gfp, bs);
		opf &= ~REQ_PREFLUSH; /* only on the first bio */

		bch2_bio_map(bio, base, chunk_size);
		bio->bi_iter.bi_sector = sector;

		if (tail) {
			/*
			 * Chain the previous (earlier-sector) bio to this one
			 * and submit it now: submission order is sector-forward
			 * so the elevator can merge with adjacent writes.
			 */
			bio_chain(tail, bio);
			submit_bio(tail);
		}
		tail = bio;

		base		+= chunk_size;
		sector		+= chunk_size >> 9;
		size		-= chunk_size;
		nr_bvecs	-= chunk_bvecs;
	}

	return tail;
}

u64 bch2_get_random_u64_below(u64 ceil)
{
	if (ceil <= U32_MAX)
		return __get_random_u32_below(ceil);

	/* this is the same (clever) algorithm as in __get_random_u32_below() */
	u64 rand = get_random_u64();
	u64 mult = ceil * rand;

	if (unlikely(mult < ceil)) {
		u64 bound;
		div64_u64_rem(-ceil, ceil, &bound);
		while (unlikely(mult < bound)) {
			rand = get_random_u64();
			mult = ceil * rand;
		}
	}

	return mul_u64_u64_shr(ceil, rand, 64);
}

void memcpy_to_bio(struct bio *dst, struct bvec_iter dst_iter, const void *src)
{
	struct bio_vec bv;
	struct bvec_iter iter;

	__bio_for_each_segment(bv, dst, iter, dst_iter) {
		void *dstp = bvec_kmap_local(&bv);
		memcpy(dstp, src, bv.bv_len);
		kunmap_local(dstp);

		src += bv.bv_len;
	}
}

void memcpy_from_bio(void *dst, struct bio *src, struct bvec_iter src_iter)
{
	struct bio_vec bv;
	struct bvec_iter iter;

	__bio_for_each_segment(bv, src, iter, src_iter) {
		void *srcp = bvec_kmap_local(&bv);
		memcpy(dst, srcp, bv.bv_len);
		kunmap_local(srcp);

		dst += bv.bv_len;
	}
}

/*
 * Copy between two bios at caller-supplied iters, advancing both. Equivalent to
 * the kernel's old bio_copy_data_iter(), which 7.2 removed in favour of the
 * whole-bio bio_copy_data().
 */
void bch2_bio_copy_data_iter(struct bio *dst, struct bvec_iter *dst_iter,
			     struct bio *src, struct bvec_iter *src_iter)
{
	while (src_iter->bi_size && dst_iter->bi_size) {
		struct bio_vec src_bv = bio_iter_iovec(src, *src_iter);
		struct bio_vec dst_bv = bio_iter_iovec(dst, *dst_iter);
		unsigned int bytes = min(src_bv.bv_len, dst_bv.bv_len);
		void *src_buf = bvec_kmap_local(&src_bv);
		void *dst_buf = bvec_kmap_local(&dst_bv);

		memcpy(dst_buf, src_buf, bytes);

		kunmap_local(dst_buf);
		kunmap_local(src_buf);

		bio_advance_iter(src, src_iter, bytes);
		bio_advance_iter(dst, dst_iter, bytes);
	}
}

/*
 * Zero-fill a bio starting at a caller-supplied iter. Equivalent to the kernel's
 * old zero_fill_bio_iter(), which 7.2 removed in favour of the whole-bio
 * zero_fill_bio().
 */
void bch2_zero_fill_bio_iter(struct bio *bio, struct bvec_iter start)
{
	struct bio_vec bv;
	struct bvec_iter iter;

	__bio_for_each_segment(bv, bio, iter, start) {
		void *p = bvec_kmap_local(&bv);

		memset(p, 0, bv.bv_len);
		kunmap_local(p);
	}
}

#ifdef __KERNEL__
#include <linux/pagemap.h>	/* folio_lock()/folio_unlock() */

/*
 * DIO read dirty-page handling. 7.2 unexported bio_set_pages_dirty() and
 * bio_check_pages_dirty(); reinstate them (the kernel's implementation) as
 * bcachefs helpers. bio_set_pages_dirty() marks the destination pages dirty on
 * the (sleepable) submit side; bio_check_pages_dirty() runs at completion, which
 * can be atomic (bio endio), so the rare "a page was cleaned mid-IO, re-dirty it"
 * case — which needs folio_lock() — is punted to a workqueue.
 */
void bch2_bio_set_pages_dirty(struct bio *bio)
{
	struct folio_iter fi;

	bio_for_each_folio_all(fi, bio) {
		folio_lock(fi.folio);
		folio_mark_dirty(fi.folio);
		folio_unlock(fi.folio);
	}
}

static void bch2_bio_dirty_fn(struct work_struct *work);

static DECLARE_WORK(bch2_bio_dirty_work, bch2_bio_dirty_fn);
static DEFINE_SPINLOCK(bch2_bio_dirty_lock);
static struct bio *bch2_bio_dirty_list;

/* runs in process context */
static void bch2_bio_dirty_fn(struct work_struct *work)
{
	struct bio *bio, *next;

	spin_lock_irq(&bch2_bio_dirty_lock);
	next = bch2_bio_dirty_list;
	bch2_bio_dirty_list = NULL;
	spin_unlock_irq(&bch2_bio_dirty_lock);

	while ((bio = next) != NULL) {
		next = bio->bi_private;

		bio_release_pages(bio, true);
		bio_put(bio);
	}
}

void bch2_bio_check_pages_dirty(struct bio *bio)
{
	struct folio_iter fi;
	unsigned long flags;

	bio_for_each_folio_all(fi, bio) {
		if (!folio_test_dirty(fi.folio))
			goto defer;
	}

	bio_release_pages(bio, false);
	bio_put(bio);
	return;
defer:
	spin_lock_irqsave(&bch2_bio_dirty_lock, flags);
	bio->bi_private = bch2_bio_dirty_list;
	bch2_bio_dirty_list = bio;
	spin_unlock_irqrestore(&bch2_bio_dirty_lock, flags);
	schedule_work(&bch2_bio_dirty_work);
}
#endif /* __KERNEL__ */

/*
 * this_cpu_add() and _cmpxchg(), for Rust where it has no asm of its own
 * (util/percpu.rs). @pcp is a _t __percpu *: the prototype Rust generates
 * can't say so, and on x86 __percpu can be an address space of its own.
 */
#define BCH2_THIS_CPU_OPS(_t)							\
void bch2_this_cpu_add_##_t(_t *pcp, _t val)					\
{										\
	this_cpu_add(*(_t __percpu *) (unsigned long) pcp, val);		\
}										\
										\
_t bch2_this_cpu_cmpxchg_##_t(_t *pcp, _t old, _t new)				\
{										\
	return this_cpu_cmpxchg(*(_t __percpu *) (unsigned long) pcp, old, new);\
}

BCH2_THIS_CPU_OPS(u32)
BCH2_THIS_CPU_OPS(u64)
#undef BCH2_THIS_CPU_OPS

/*
 * static_branch_unlikely(), for Rust where it has no asm of its own
 * (util/jump_label.rs): out of line, @key isn't a constant the asm can name -
 * the key's count, as static_branch_unlikely() reads it without
 * CONFIG_JUMP_LABEL.
 */
bool bch2_static_branch_unlikely(struct static_key_false *key)
{
	return static_key_enabled(&key->key);
}

/* ...per_cpu_ptr(), and the possible CPUs: the one after @cpu, -1 at the end */
void *bch2_per_cpu_ptr(void *pcp, unsigned cpu)
{
	return per_cpu_ptr((void __percpu *) (unsigned long) pcp, cpu);
}

int bch2_cpu_possible_next(int cpu)
{
#ifdef __KERNEL__
	cpu = cpumask_next(cpu, cpu_possible_mask);
	return cpu < nr_cpu_ids ? cpu : -1;
#else
	return cpu + 1 < bch_percpu_nr_cpus ? cpu + 1 : -1;
#endif
}

/* ...this CPU's: its copy, preemption and migration, and its id */
void *bch2_this_cpu_ptr(void *pcp)
{
	return this_cpu_ptr((void __percpu *) (unsigned long) pcp);
}

void bch2_preempt_disable(void)
{
	preempt_disable();
}

void bch2_preempt_enable(void)
{
	preempt_enable();
}

unsigned bch2_raw_smp_processor_id(void)
{
	return raw_smp_processor_id();
}

void bch2_migrate_disable(void)
{
	migrate_disable();
}

void bch2_migrate_enable(void)
{
	migrate_enable();
}

/* ...and a task's: its pid, and the CPU it's to be woken on */
int bch2_task_pid(struct task_struct *task)
{
	return task->pid;
}

/*
 * Compiled out under CONFIG_SCHED_ALT (BMQ/PDS): those schedulers don't have
 * select_task_rq_fair() and drop task_struct.wake_cpu.
 */
void bch2_set_current_wake_cpu(int cpu)
{
#if defined(__KERNEL__) && !defined(CONFIG_SCHED_ALT)
	WRITE_ONCE(current->wake_cpu, cpu);
#endif
}

#if !defined(__KERNEL__) || LINUX_VERSION_CODE >= KERNEL_VERSION(6,19,0)
void *mempool_kvmalloc(gfp_t gfp_mask, void *pool_data)
{
	size_t size = (size_t)pool_data;
	return kvmalloc(size, gfp_mask);
}

void mempool_kvfree(void *element, void *pool_data)
{
	kvfree(element);
}
#endif

__sched int bch2_bit_wait_io_timeout(struct wait_bit_key *word, int mode)
{
	unsigned long now = jiffies;

	if (time_after_eq(now, word->timeout))
		return -EAGAIN;
	io_schedule_timeout(word->timeout - now);
	if (signal_pending_state(mode, current))
		return -EINTR;

	return 0;
}

/*
 * For Rust: the kernel's Rust allocator API has no __GFP_NOFAIL. Out of line,
 * as locking/locking.c's shims are, for the same reason.
 */
void *rust_kzalloc_nofail(size_t size)
{
	return kzalloc(size, GFP_KERNEL|__GFP_NOFAIL);
}

void rust_kfree(const void *p)
{
	kfree(p);
}

/* For util/rcu.rs: kvfree_rcu() is a macro, of a struct's rcu_head field */
void *rust_kvzalloc(size_t size)
{
	return kvzalloc(size, GFP_KERNEL);
}

void rust_kvfree(void *p)
{
	kvfree(p);
}

/*
 * kvfree_rcu(p, field), for a head the macro can't name: anywhere in @p. As a
 * void *, as the macro casts it: newer kernels take a struct kvfree_rcu_head,
 * no bigger than an rcu_head.
 */
void rust_kvfree_rcu(void *p, struct rcu_head *head)
{
#ifdef __KERNEL__
	kvfree_call_rcu((void *) head, p);
#else
	kvfree_rcu(p, head);
#endif
}

/* For util/bio/pages.rs: __get_free_pages() is a macro */
void *rust_bio_pages_alloc(size_t size, gfp_t gfp)
{
#ifdef __KERNEL__
	/* page aligned: we don't know the device's dma alignment */
	return (void *) __get_free_pages(gfp, get_order(size));
#else
	return kmalloc(size, gfp);
#endif
}

void rust_bio_pages_free(void *p, size_t size)
{
	free_pages((unsigned long) p, get_order(size));
}

void rust_kfree_rcu_mightsleep(void *p)
{
	kfree_rcu_mightsleep(p);
}

/* For util/kernel.rs: a static key */
bool rust_mem_alloc_profiling_enabled(void)
{
	return mem_alloc_profiling_enabled();
}

/*
 * For Rust, what it can't call itself: static inlines and macros, and in
 * places which of the two differs between the kernel and userspace.
 */
u64 bch2_local_clock(void)
{
	return local_clock();
}

void bch2_cond_resched(void)
{
	cond_resched();
}

void bch2_msleep(unsigned ms)
{
	msleep(ms);
}

bool bch2_fatal_signal_pending(void)
{
	return fatal_signal_pending(current);
}

u64 bch2_ktime_get_ns(void)
{
	return ktime_get_ns();
}

#ifdef __KERNEL__
unsigned bch2_num_online_cpus(void)
{
	return num_online_cpus();
}
#endif

bool bch2_bdev_rot(struct block_device *bdev)
{
	return bdev_rot(bdev);
}

int bch2_prt_current_backtrace(struct printbuf *out, unsigned skipnr, gfp_t gfp)
{
	return bch2_prt_task_backtrace(out, current, skipnr, gfp);
}

u64 bch2_get_random_u64(void)
{
	return get_random_u64();
}

/* queue_work() is a static inline in the kernel, a function in userspace: */
bool bch2_queue_work(struct workqueue_struct *wq, struct work_struct *work)
{
	return queue_work(wq, work);
}

/*
 * For DelayedWork (util/workqueue.rs). The kernel build initializes with a
 * lockdep class per Rust call site, as the kernel crate does - this is the
 * shim's INIT_DELAYED_WORK().
 */
void bch2_init_delayed_work(struct delayed_work *dwork, work_func_t func)
{
	INIT_DELAYED_WORK(dwork, func);
}

bool bch2_queue_delayed_work(struct workqueue_struct *wq, struct delayed_work *dwork,
			     unsigned long delay)
{
	return queue_delayed_work(wq, dwork, delay);
}

/* For util/workqueue.rs: alloc_workqueue() is a macro, its name a format */
struct workqueue_struct *bch2_alloc_workqueue(const char *name, unsigned flags, int max_active)
{
	return alloc_workqueue("%s", flags, max_active, name);
}

/* util/workqueue.rs's WqFlags are C's WQ_* */
static_assert(BCH_WQ_UNBOUND		== WQ_UNBOUND);
static_assert(BCH_WQ_FREEZABLE		== WQ_FREEZABLE);
static_assert(BCH_WQ_MEM_RECLAIM	== WQ_MEM_RECLAIM);
static_assert(BCH_WQ_HIGHPRI		== WQ_HIGHPRI);
static_assert(BCH_WQ_CPU_INTENSIVE	== WQ_CPU_INTENSIVE);
static_assert(BCH_WQ_PERCPU		== WQ_PERCPU || !WQ_PERCPU);

/* For util/kthread.rs: kthread_create() is a macro in the kernel, a function in userspace */
struct task_struct *bch2_kthread_create(int (*fn)(void *), void *data, const char *name)
{
	return kthread_create(fn, data, "%s", name);
}

int bch2_kthread_stop(struct task_struct *p)
{
	return kthread_stop(p);
}

bool bch2_kthread_should_stop(void)
{
	return (current->flags & PF_KTHREAD) && kthread_should_stop();
}

/*
 * For util/sleep.rs: current, set_current_state() and
 * signal_pending_state() are macros, or inlines over current.
 */
struct task_struct *bch2_current(void)
{
	return current;
}

void bch2_set_current_state(unsigned int state)
{
	set_current_state(state);
}

void bch2_set_current_state_running(void)
{
	__set_current_state(TASK_RUNNING);
}

bool bch2_signal_pending_state(unsigned int state)
{
	return signal_pending_state(state, current);
}

#ifdef CONFIG_SMP
#include <linux/sched/rt.h>

/* For locking/six.rs's optimistic spin: inlines over current and its owner */
bool bch2_need_resched(void)
{
	return need_resched();
}

bool bch2_owner_on_cpu(struct task_struct *owner)
{
	return owner_on_cpu(owner);
}

bool bch2_rt_or_dl_task_current(void)
{
	return rt_or_dl_task(current);
}

u64 bch2_sched_clock(void)
{
	return sched_clock();
}
#endif

/* For util/task.rs: INIT_WORK() is a macro, with a lockdep class in the kernel */
void bch2_init_work(struct work_struct *work, work_func_t func)
{
	INIT_WORK(work, func);
}

/* For util/par.rs: set_worker_desc() takes a format */
void bch2_set_worker_desc(const char *desc)
{
	set_worker_desc("%s", desc);
}

/* capable() is a function in the kernel, a macro (true) in userspace's blkdev.h */
bool bch2_capable(int cap)
{
	return capable(cap);
}

/* For data_structures/rhashtable.rs, to which struct bucket_table is opaque */
const struct bucket_table *bch2_rht_table(const struct rhashtable *ht)
{
	/* lockdep_rht_mutex_is_held() only reads it, but isn't const */
	return rht_dereference_rcu(ht->tbl, (struct rhashtable *) ht);
}

unsigned bch2_rht_nr_buckets(const struct bucket_table *tbl)
{
	return tbl->size;
}

void **bch2_rht_bucket(const struct bucket_table *tbl, unsigned hash)
{
	return (void **) rht_bucket(tbl, hash);
}

/* For util/debugfs.rs: debugfs_create_file() is, in the kernel, a _Generic() over its fops' type */
struct dentry *bch2_debugfs_create_file(const char *name, umode_t mode, struct dentry *parent,
					void *data, const struct file_operations *fops)
{
	return debugfs_create_file(name, mode, parent, data, fops);
}

#ifdef __KERNEL__
#include <linux/cpumask.h>
#include <linux/numa.h>

/* NUMA_NO_NODE is a macro, nr_cpu_ids a variable - or a macro, without SMP */
unsigned bch2_cpumask_local_spread(unsigned i)
{
	return cpumask_local_spread(i, NUMA_NO_NODE);
}

unsigned bch2_nr_cpu_ids(void)
{
	return nr_cpu_ids;
}
#endif
