#ifndef __TOOLS_LINUX_PERCPU_REFCOUNT_H
#define __TOOLS_LINUX_PERCPU_REFCOUNT_H

#include <linux/atomic.h>
#include <linux/kernel.h>
#include <linux/percpu.h>

struct percpu_ref;
typedef void (percpu_ref_func_t)(struct percpu_ref *);

/* flags set in the lower bits of percpu_ref->percpu_count_ptr */
enum {
	__PERCPU_REF_ATOMIC	= 1LU << 0,	/* operating in atomic mode */
	__PERCPU_REF_DEAD	= 1LU << 1,	/* (being) killed */
	__PERCPU_REF_ATOMIC_DEAD = __PERCPU_REF_ATOMIC | __PERCPU_REF_DEAD,

	__PERCPU_REF_FLAG_BITS	= 2,
};

/* @flags for percpu_ref_init() */
enum {
	PERCPU_REF_INIT_ATOMIC	= 1 << 0,

	/*
	 * Start dead w/ ref == 0 in atomic mode.  Must be revived with
	 * percpu_ref_reinit() before used.  Implies INIT_ATOMIC.
	 */
	PERCPU_REF_INIT_DEAD	= 1 << 1,
};

struct percpu_ref {
	atomic_long_t		count;
	percpu_ref_func_t	*release;
	percpu_ref_func_t	*confirm_switch;
};

static inline void percpu_ref_exit(struct percpu_ref *ref) {}

static inline int __must_check percpu_ref_init(struct percpu_ref *ref,
				 percpu_ref_func_t *release, unsigned int flags,
				 gfp_t gfp)
{
	unsigned long start_count = 0;

	if (!(flags & PERCPU_REF_INIT_DEAD))
		start_count++;

	atomic_long_set(&ref->count, start_count);

	ref->release = release;
	return 0;
}

/**
 * percpu_ref_get_many - increment a percpu refcount
 * @ref: percpu_ref to get
 * @nr: number of references to get
 *
 * Analogous to atomic_long_add().
 *
 * This function is safe to call as long as @ref is between init and exit.
 */
static inline void percpu_ref_get_many(struct percpu_ref *ref, unsigned long nr)
{
	atomic_long_add(nr, &ref->count);
}

/**
 * percpu_ref_get - increment a percpu refcount
 * @ref: percpu_ref to get
 *
 * Analogous to atomic_long_inc().
 *
 * This function is safe to call as long as @ref is between init and exit.
 */
static inline void percpu_ref_get(struct percpu_ref *ref)
{
	percpu_ref_get_many(ref, 1);
}

/**
 * percpu_ref_tryget - try to increment a percpu refcount
 * @ref: percpu_ref to try-get
 *
 * Increment a percpu refcount unless its count already reached zero.
 * Returns %true on success; %false on failure.
 *
 * This function is safe to call as long as @ref is between init and exit.
 */
static inline bool percpu_ref_tryget(struct percpu_ref *ref)
{
	return atomic_long_inc_not_zero(&ref->count);
}

/**
 * percpu_ref_tryget_live - try to increment a live percpu refcount
 * @ref: percpu_ref to try-get
 *
 * Increment a percpu refcount unless it has already been killed.  Returns
 * %true on success; %false on failure.
 *
 * Completion of percpu_ref_kill() in itself doesn't guarantee that this
 * function will fail.  For such guarantee, percpu_ref_kill_and_confirm()
 * should be used.  After the confirm_kill callback is invoked, it's
 * guaranteed that no new reference will be given out by
 * percpu_ref_tryget_live().
 *
 * This function is safe to call as long as @ref is between init and exit.
 */
static inline bool percpu_ref_tryget_live(struct percpu_ref *ref)
{
	return atomic_long_inc_not_zero(&ref->count);
}

/**
 * percpu_ref_put_many - decrement a percpu refcount
 * @ref: percpu_ref to put
 * @nr: number of references to put
 *
 * Decrement the refcount, and if 0, call the release function (which was passed
 * to percpu_ref_init())
 *
 * This function is safe to call as long as @ref is between init and exit.
 */
static inline void percpu_ref_put_many(struct percpu_ref *ref, unsigned long nr)
{
	if (unlikely(atomic_long_sub_and_test(nr, &ref->count)))
		ref->release(ref);
}

/**
 * percpu_ref_put - decrement a percpu refcount
 * @ref: percpu_ref to put
 *
 * Decrement the refcount, and if 0, call the release function (which was passed
 * to percpu_ref_init())
 *
 * This function is safe to call as long as @ref is between init and exit.
 */
static inline void percpu_ref_put(struct percpu_ref *ref)
{
	percpu_ref_put_many(ref, 1);
}

static inline void percpu_ref_reinit(struct percpu_ref *ref)
{
	percpu_ref_get(ref);
}

/**
 * percpu_ref_kill - drop the initial ref
 * @ref: percpu_ref to kill
 *
 * Must be used to drop the initial ref on a percpu refcount; must be called
 * precisely once before shutdown.
 */
static inline void percpu_ref_kill(struct percpu_ref *ref)
{
	percpu_ref_put(ref);
}

/**
 * percpu_ref_is_zero - test whether a percpu refcount reached zero
 * @ref: percpu_ref to test
 *
 * Returns %true if @ref reached zero.
 *
 * This function is safe to call as long as @ref is between init and exit.
 */
static inline bool percpu_ref_is_zero(struct percpu_ref *ref)
{
	return !atomic_long_read(&ref->count);
}

static inline bool percpu_ref_is_dying(struct percpu_ref *ref)
{
	return percpu_ref_is_zero(ref);
}

#endif /* __TOOLS_LINUX_PERCPU_REFCOUNT_H */
