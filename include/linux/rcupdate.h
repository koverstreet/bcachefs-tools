#ifndef __TOOLS_LINUX_RCUPDATE_H
#define __TOOLS_LINUX_RCUPDATE_H

#include <urcu.h>
#include <linux/compiler.h>
#include <linux/cleanup.h>

#define ULONG_CMP_GE(a, b)      (ULONG_MAX / 2 >= (a) - (b))

#define rcu_dereference_check(p, c)	rcu_dereference(p)
#define rcu_dereference_raw(p)		rcu_dereference(p)
#define rcu_dereference_protected(p, c)	rcu_dereference(p)
#define rcu_access_pointer(p)		READ_ONCE(p)

/*
 * These defer the free to the end of a grace period, as the kernel versions
 * do. They previously expanded to a plain kfree(), which is a use-after-free
 * anywhere a reader is concurrent - and shared fs/ code is written against
 * the kernel's semantics: bch2_snapshot_table_make() frees the snapshot table
 * this way while every rcu_dereference(c->snapshots.table) reader is live, and
 * bch2_btree_bkey_cached_common_lock_held()'s neighbours in btree/interior.c
 * carry a comment saying a concurrent lookup will memcmp freed memory
 * otherwise.
 *
 * The rcu_head embedded in the object can't be used directly: call_rcu() hands
 * the callback &obj->rcu, and free() needs the base of the allocation, which
 * isn't recoverable from that without knowing the field's offset. So carry the
 * pointer in a wrapper.
 *
 * If the wrapper can't be allocated, block for a grace period rather than free
 * under readers - slow, but the alternative is the bug this replaced.
 */
struct rcu_free_wrapper {
	struct rcu_head	rcu;
	void		*p;
};

static inline void rcu_free_wrapper_cb(struct rcu_head *head)
{
	struct rcu_free_wrapper *w =
		container_of(head, struct rcu_free_wrapper, rcu);

	free(w->p);
	free(w);
}

static inline void rcu_free_ptr(void *p)
{
	if (!p)
		return;

	struct rcu_free_wrapper *w = malloc(sizeof(*w));
	if (w) {
		w->p = p;
		call_rcu(&w->rcu, rcu_free_wrapper_cb);
	} else {
		synchronize_rcu();
		free(p);
	}
}

#define kfree_rcu(ptr, rcu_head)	rcu_free_ptr(ptr)
#define kfree_rcu_mightsleep(ptr)	rcu_free_ptr(ptr)
#define kvfree_rcu(ptr, rcu_head)	rcu_free_ptr(ptr)
#define kvfree_rcu_mightsleep(ptr)	rcu_free_ptr(ptr)

#define RCU_INIT_POINTER(p, v)		WRITE_ONCE(p, v)

/* Has the specified rcu_head structure been handed to call_rcu()? */

/**
 * rcu_head_init - Initialize rcu_head for rcu_head_after_call_rcu()
 * @rhp: The rcu_head structure to initialize.
 *
 * If you intend to invoke rcu_head_after_call_rcu() to test whether a
 * given rcu_head structure has already been passed to call_rcu(), then
 * you must also invoke this rcu_head_init() function on it just after
 * allocating that structure.  Calls to this function must not race with
 * calls to call_rcu(), rcu_head_after_call_rcu(), or callback invocation.
 */
static inline void rcu_head_init(struct rcu_head *rhp)
{
	rhp->func = (void *)~0L;
}

static inline bool
rcu_head_after_call_rcu(struct rcu_head *rhp,
			void (*f)(struct rcu_head *head))
{
	void (*func)(struct rcu_head *head) = READ_ONCE(rhp->func);

	if (func == f)
		return true;
	return false;
}

DEFINE_LOCK_GUARD_0(rcu,
	do {
		rcu_read_lock();
		/*
		 * sparse doesn't call the cleanup function,
		 * so just release immediately and don't track
		 * the context. We don't need to anyway, since
		 * the whole point of the guard is to not need
		 * the explicit unlock.
		 */
		__release(RCU);
	} while (0),
	rcu_read_unlock())

#endif /* __TOOLS_LINUX_RCUPDATE_H */
