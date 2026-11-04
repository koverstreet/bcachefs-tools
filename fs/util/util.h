/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_UTIL_H
#define _BCACHEFS_UTIL_H

#include <linux/bio.h>
#include <linux/blkdev.h>
#ifdef __KERNEL__
#include <linux/capability.h>
#endif
#include <linux/errno.h>
#include <linux/freezer.h>
#include <linux/kernel.h>
#include <linux/sched/clock.h>
#include <linux/llist.h>
#include <linux/log2.h>
#include <linux/percpu.h>
#include <linux/preempt.h>
#include <linux/random.h>
#include <linux/ratelimit.h>
#include <linux/slab.h>
#include <linux/sort.h>
#include <linux/version.h>
#include <linux/vmalloc.h>
#include <linux/workqueue.h>

#include "closure.h"
#include "mean_and_variance.h"

#include "data_structures/darray.h"
#include "time_stats.h"

#include "printbuf.h"
#ifdef __KERNEL__
#include <linux/cpumask.h>
#include <linux/numa.h>
#else
#include <uuid/uuid.h>
#endif
#include <linux/uuid.h>
#include <linux/sched/mm.h>
#include "util/util_gen.h"
#include "util/backtrace_gen.h"
#include "util/bio_gen.h"

#ifdef CONFIG_BCACHEFS_DEBUG
#define EBUG_ON(cond)		BUG_ON(cond)
#else
#define EBUG_ON(cond)
#endif

#if __BYTE_ORDER__ == __ORDER_LITTLE_ENDIAN__
#define CPU_BIG_ENDIAN		0
#elif __BYTE_ORDER__ == __ORDER_BIG_ENDIAN__
#define CPU_BIG_ENDIAN		1
#endif

/* type hackery */

#define type_is_exact(_val, _type)					\
	__builtin_types_compatible_p(typeof(_val), _type)

#define type_is(_val, _type)						\
	(__builtin_types_compatible_p(typeof(_val), _type) ||		\
	 __builtin_types_compatible_p(typeof(_val), const _type))

#if defined(__KERNEL__) && LINUX_VERSION_CODE < KERNEL_VERSION(6,18,0)
static inline struct bio_vec *bio_inline_vecs(struct bio *bio)
{
	return (struct bio_vec *)(bio + 1);
}
#endif

/*
 * 7.1 introduced bdev_rot() and removed bdev_nonrot(). Shim bdev_rot for
 * older kernels and userspace so call sites can always use bdev_rot.
 */
#if !defined(__KERNEL__) || LINUX_VERSION_CODE < KERNEL_VERSION(7,0,0)
static inline bool bdev_rot(struct block_device *bdev)
{
	return !bdev_nonrot(bdev);
}
#endif

DEFINE_FREE(bio_put, struct bio *, if (_T) bio_put(_T))

#include "printbuf.h"

#define prt_vprintf(_out, ...)		bch2_prt_vprintf(_out, __VA_ARGS__)
#define prt_printf(_out, ...)		bch2_prt_printf(_out, __VA_ARGS__)
#define printbuf_exit(_buf)		bch2_printbuf_exit(_buf)

#define printbuf_tabstops_reset(_buf)	bch2_printbuf_tabstops_reset(_buf)
#define printbuf_tabstop_push(_buf, _n)	bch2_printbuf_tabstop_push(_buf, _n)

#define printbuf_indent_add(_out, _n)	bch2_printbuf_indent_add(_out, _n)
#define printbuf_indent_add_nextline(_out, _n)	bch2_printbuf_indent_add_nextline(_out, _n)
#define printbuf_indent_sub(_out, _n)	bch2_printbuf_indent_sub(_out, _n)

#define prt_newline(_out)		bch2_prt_newline(_out)
#define prt_tab(_out)			bch2_prt_tab(_out)
#define prt_tab_rjust(_out)		bch2_prt_tab_rjust(_out)

#define prt_u64(_out, _v)		prt_printf(_out, "%llu", (u64) (_v))
#define prt_human_readable_u64(...)	bch2_prt_human_readable_u64(__VA_ARGS__)
#define prt_string_option(...)		bch2_prt_string_option(__VA_ARGS__)
#define prt_bitflags(...)		bch2_prt_bitflags(__VA_ARGS__)

bool bch2_is_zero(const void *, size_t);

#define ewma_add(ewma, val, weight)					\
({									\
	typeof(ewma) _ewma = (ewma);					\
	typeof(weight) _weight = (weight);				\
									\
	(((_ewma << _weight) - _ewma) + (val)) >> _weight;		\
})

#define container_of_or_null(ptr, type, member)				\
({									\
	typeof(ptr) _ptr = ptr;						\
	_ptr ? container_of(_ptr, type, member) : NULL;			\
})

static inline struct list_head *list_pop(struct list_head *head)
{
	if (list_empty(head))
		return NULL;

	struct list_head *ret = head->next;
	list_del_init(ret);
	return ret;
}

#define list_pop_entry(head, type, member)		\
	container_of_or_null(list_pop(head), type, member)

struct bio *bch2_bio_map_and_chain(struct block_device *, void *, size_t,
				   sector_t, blk_opf_t, gfp_t,
				   struct bio_set *);

#define closure_bio_submit(bio, cl)					\
do {									\
	closure_get(cl);						\
	submit_bio(bio);						\
} while (0)

#define kthread_wait_freezable(cond)					\
({									\
	int _ret = 0;							\
	while (1) {							\
		set_current_state(TASK_INTERRUPTIBLE);			\
		if (kthread_should_stop()) {				\
			_ret = -1;					\
			break;						\
		}							\
									\
		if (cond)						\
			break;						\
									\
		schedule();						\
		try_to_freeze();					\
	}								\
	set_current_state(TASK_RUNNING);				\
	_ret;								\
})

/*
 * All-ones mask of width @bits, defined for the full range 0..64 — unlike
 * (1ULL << bits) - 1 (UB at 64) or ~0ULL >> (64 - bits) (UB at 0).
 * Compiles branchless (cmov).
 */
static inline u64 u64_bitmask(unsigned bits)
{
	return bits ? ~0ULL >> (64 - bits) : 0;
}

void bch2_bio_set_pages_dirty(struct bio *);
void bch2_bio_check_pages_dirty(struct bio *);

#ifdef CONFIG_BCACHEFS_DEBUG
void bch2_corrupt_bio(struct bio *);

static inline void bch2_maybe_corrupt_bio(struct bio *bio, unsigned ratio)
{
	if (ratio && !get_random_u32_below(ratio))
		bch2_corrupt_bio(bio);
}
#else
#define bch2_maybe_corrupt_bio(...)	do {} while (0)
#endif

static inline void __memcpy_u64s(void *dst, const void *src,
				 unsigned u64s)
{
	unsafe_memcpy(dst, src, u64s * 8,
		      "dst is a u64s array sized by the caller, not a single field");
}

/*
 * For small, bounded copies (a key, a handful of u64s): the open-coded loop is
 * tighter codegen than a memcpy() call / its inline expansion. For larger or
 * variable-size copies (key + value, bulk), use memcpy_u64s() so we get the
 * fast-string path.
 */
static inline void memcpy_u64s_small(void *dst, const void *src,
				     unsigned u64s)
{
	u64 *d = dst;
	const u64 *s = src;

	while (u64s--)
		*d++ = *s++;
}

static inline void memcpy_u64s(void *dst, const void *src,
			       unsigned u64s)
{
	EBUG_ON(!(dst >= src + u64s * sizeof(u64) ||
		 dst + u64s * sizeof(u64) <= src));

	__memcpy_u64s(dst, src, u64s);
}

/*
 * Fortify can't see the real object size for u64s-array copies, same as
 * __memcpy_u64s() - but there's no unsafe_memmove(), so go underneath
 * fortify by hand:
 */
static inline void __memmove_u64s(void *dst, const void *src,
				  unsigned u64s)
{
#ifdef _LINUX_FORTIFY_STRING_H_
	__underlying_memmove(dst, src, u64s * 8);
#else
	memmove(dst, src, u64s * 8);
#endif
}

static inline void memmove_u64s_down(void *dst, const void *src,
				     unsigned u64s)
{
	EBUG_ON(dst > src);

	__memmove_u64s(dst, src, u64s);
}

static inline void __memmove_u64s_down_small(void *dst, const void *src,
				       unsigned u64s)
{
	/* dst <= src: a forward copy can't clobber unread source */
	memcpy_u64s_small(dst, src, u64s);
}

static inline void memmove_u64s_down_small(void *dst, const void *src,
				     unsigned u64s)
{
	EBUG_ON(dst > src);

	__memmove_u64s_down_small(dst, src, u64s);
}

static inline void __memmove_u64s_up_small(void *_dst, const void *_src,
					   unsigned u64s)
{
	u64 *dst = (u64 *) _dst + u64s;
	u64 *src = (u64 *) _src + u64s;

	while (u64s--)
		*--dst = *--src;
}

static inline void memmove_u64s_up_small(void *dst, const void *src,
					 unsigned u64s)
{
	EBUG_ON(dst < src);

	__memmove_u64s_up_small(dst, src, u64s);
}

/* just the memmove, doesn't update @_nr */
#define __array_insert_item(_array, _nr, _pos)				\
	memmove(&(_array)[(_pos) + 1],					\
		&(_array)[(_pos)],					\
		sizeof((_array)[0]) * ((_nr) - (_pos)))

#define array_insert_item(_array, _nr, _pos, _new_item)			\
do {									\
	__array_insert_item(_array, _nr, _pos);				\
	(_nr)++;							\
	(_array)[(_pos)] = (_new_item);					\
} while (0)

#define array_remove_items(_array, _nr, _pos, _nr_to_remove)		\
do {									\
	(_nr) -= (_nr_to_remove);					\
	memmove(&(_array)[(_pos)],					\
		&(_array)[(_pos) + (_nr_to_remove)],			\
		sizeof((_array)[0]) * ((_nr) - (_pos)));		\
} while (0)

#define array_remove_item(_array, _nr, _pos)				\
	array_remove_items(_array, _nr, _pos, 1)

static inline void __move_gap(void *array, size_t element_size,
			      size_t nr, size_t size,
			      size_t old_gap, size_t new_gap)
{
	size_t gap_end = old_gap + size - nr;

	if (new_gap < old_gap) {
		size_t move = old_gap - new_gap;

		memmove(array + element_size * (gap_end - move),
			array + element_size * (old_gap - move),
				element_size * move);
	} else if (new_gap > old_gap) {
		size_t move = new_gap - old_gap;

		memmove(array + element_size * old_gap,
			array + element_size * gap_end,
				element_size * move);
	}
}

/* Move the gap in a gap buffer: */
#define move_gap(_d, _new_gap)						\
do {									\
	BUG_ON(_new_gap > (_d)->nr);					\
	BUG_ON((_d)->gap > (_d)->nr);					\
									\
	__move_gap((_d)->data, sizeof((_d)->data[0]),			\
		   (_d)->nr, (_d)->size, (_d)->gap, _new_gap);		\
	(_d)->gap = _new_gap;						\
} while (0)

#define bubble_sort(_base, _nr, _cmp)					\
do {									\
	ssize_t _i, _last;						\
	bool _swapped = true;						\
									\
	for (_last= (ssize_t) (_nr) - 1; _last > 0 && _swapped; --_last) {\
		_swapped = false;					\
		for (_i = 0; _i < _last; _i++)				\
			if (_cmp((_base)[_i], (_base)[_i + 1]) > 0) {	\
				swap((_base)[_i], (_base)[_i + 1]);	\
				_swapped = true;			\
			}						\
	}								\
} while (0)

#define per_cpu_sum(_p)							\
({									\
	TYPEOF_UNQUAL(*_p) _ret = 0;					\
									\
	int cpu;							\
	for_each_possible_cpu(cpu)					\
		_ret += *per_cpu_ptr(_p, cpu);				\
	_ret;								\
})

static inline u64 percpu_u64_get(u64 __percpu *src)
{
	return per_cpu_sum(src);
}

/*
 * For counters of signed deltas, added to on any cpu: a sum taken while they
 * change can be negative, which as a u64 is nearly 2^64.
 */
static inline u64 percpu_u64_get_nonnegative(u64 __percpu *src)
{
	return max_t(s64, percpu_u64_get(src), 0);
}

#include <linux/uuid.h>

static inline void mod_bit(long nr, volatile unsigned long *addr, bool v)
{
	if (v)
		set_bit(nr, addr);
	else
		clear_bit(nr, addr);
}

static inline void __set_bit_le64(size_t bit, __le64 *addr)
{
	addr[bit / 64] |= cpu_to_le64(BIT_ULL(bit % 64));
}

static inline void __clear_bit_le64(size_t bit, __le64 *addr)
{
	addr[bit / 64] &= ~cpu_to_le64(BIT_ULL(bit % 64));
}

static inline bool test_bit_le64(size_t bit, __le64 *addr)
{
	return (addr[bit / 64] & cpu_to_le64(BIT_ULL(bit % 64))) != 0;
}

static inline bool __test_and_set_bit_le64(size_t bit, __le64 *addr)
{
	bool ret = test_bit_le64(bit, addr);
	__set_bit_le64(bit, addr);
	return ret;
}

static inline void memcpy_swab(void *_dst, void *_src, size_t len)
{
	u8 *dst = _dst + len;
	u8 *src = _src;

	while (len--)
		*--dst = *src++;
}

#define set_flags(_map, _in, _out)					\
do {									\
	unsigned _i;							\
									\
	for (_i = 0; _i < ARRAY_SIZE(_map); _i++)			\
		if ((_in) & (1 << _i))					\
			(_out) |= _map[_i];				\
		else							\
			(_out) &= ~_map[_i];				\
} while (0)

#define try(_do)							\
do {									\
	typeof(_do) _ret = (_do);					\
	if (unlikely(_ret))						\
		return _ret;						\
} while (0)

#define errptr_try(_do)							\
({									\
	typeof(_do) _ret = (_do);					\
	if (IS_ERR(_ret))						\
		return PTR_ERR(_ret);					\
	_ret;								\
})

#include <linux/sched/mm.h>

/*
 * Upstream's, until the revert gave their bits back to sched.h's holes - so
 * the page allocator doesn't see them: our Rust allocation entry points apply
 * them (util/alloc.rs, current_gfp_context()), and nothing else does.
 */
#ifndef PF_MEMALLOC_NORECLAIM
#if !defined(PF__HOLE__00800000) || !defined(PF__HOLE__01000000)
#error "PF_MEMALLOC_NORECLAIM, PF_MEMALLOC_NOWARN: PF__HOLE__00800000 and PF__HOLE__01000000 are taken - pick two of sched.h's PF__HOLE__* still free"
#endif
#define PF_MEMALLOC_NORECLAIM	PF__HOLE__00800000
#define PF_MEMALLOC_NOWARN	PF__HOLE__01000000
#endif

DEFINE_CLASS(memalloc_flags, struct memalloc_flags,
	     memalloc_flags_restore(_T.flags),
	     (struct memalloc_flags) { memalloc_flags_save(_flags) },
	     unsigned _flags)
__DEFINE_CLASS_IS_CONDITIONAL(memalloc_flags, false);

static inline void *class_memalloc_flags_lock_ptr(class_memalloc_flags_t *_T)
{
	return _T;
}

#if !defined(__KERNEL__) || LINUX_VERSION_CODE >= KERNEL_VERSION(6,19,0)
void *mempool_kvmalloc(gfp_t gfp_mask, void *pool_data);
void mempool_kvfree(void *element, void *pool_data);
#endif

#include <linux/wait_bit.h>

int bch2_bit_wait_io_timeout(struct wait_bit_key *, int);

/*
 * wait_on_bit_io_timeout() - missing from <linux/wait_bit.h>; same as
 * wait_on_bit_timeout() but uses io_schedule_timeout() so the wait is
 * accounted as iowait.
 */
static inline int wait_on_bit_io_timeout(unsigned long *word, int bit,
					 unsigned mode, unsigned long timeout)
{
	might_sleep();
	if (!test_bit_acquire(bit, word))
		return 0;
	return out_of_line_wait_on_bit_timeout(word, bit,
					       bch2_bit_wait_io_timeout,
					       mode, timeout);
}

/*
 * bch2_alloc_percpu_init(): run init() on every per-cpu instance of a
 * dynamically-allocated percpu variable.
 *
 * Kernel: iterates for_each_possible_cpu and calls init() once per cpu
 * at allocation time.
 *
 * Userspace: alloc_percpu() returns an offset into a per-thread chunk;
 * threads come and go dynamically, so init() must also be called for
 * each thread that's spawned later. The userspace shim stashes the
 * (init, ctx) pair alongside the percpu variable so it can call init()
 * for every existing chunk now and every future chunk at thread
 * creation time.
 *
 * The init function takes (per-instance ptr, caller ctx, cpu id).
 */
#ifdef __KERNEL__
#define bch2_alloc_percpu_init(_pcv, _init, _ctx)			\
do {									\
	int _cpu;							\
	for_each_possible_cpu(_cpu)					\
		(_init)(per_cpu_ptr((_pcv), _cpu), (_ctx), _cpu);	\
} while (0)
#else
void __bch2_alloc_percpu_init(void *pcv,
			      void (*init)(void *, void *, unsigned),
			      void *ctx);
#define bch2_alloc_percpu_init(_pcv, _init, _ctx)			\
	__bch2_alloc_percpu_init((_pcv),				\
		(void (*)(void *, void *, unsigned))(_init), (_ctx))
#endif

static inline u64 system_totalram_bytes(void)
{
	struct sysinfo i;
	si_meminfo(&i);

	return i.totalram * i.mem_unit;
}

#endif /* _BCACHEFS_UTIL_H */
