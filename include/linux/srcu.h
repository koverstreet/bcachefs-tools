#ifndef __TOOLS_LINUX_SRCU_H
#define __TOOLS_LINUX_SRCU_H

#include <linux/cleanup.h>
#include <linux/rcupdate.h>

#define NUM_ACTIVE_RCU_POLL_OLDSTATE	2

typedef void (*rcu_callback_t)(struct rcu_head *head);

static inline struct urcu_gp_poll_state get_state_synchronize_rcu()
{
	return start_poll_synchronize_rcu();
}

struct srcu_struct {
};

static inline void srcu_read_unlock(struct srcu_struct *ssp, int idx) {}

static inline int srcu_read_lock(struct srcu_struct *ssp)
{
	return 0;
}

static inline bool poll_state_synchronize_srcu(struct srcu_struct *ssp, struct urcu_gp_poll_state cookie)
{
	return poll_state_synchronize_rcu(cookie);
}

static inline struct urcu_gp_poll_state start_poll_synchronize_srcu(struct srcu_struct *ssp)
{
	return start_poll_synchronize_rcu();
}

static inline struct urcu_gp_poll_state get_state_synchronize_srcu(struct srcu_struct *ssp)
{
	return get_state_synchronize_rcu();
}

static inline void synchronize_srcu_expedited(struct srcu_struct *ssp) {}

static inline void srcu_barrier(struct srcu_struct *ssp) {}

static inline void cleanup_srcu_struct(struct srcu_struct *ssp) {}

static inline void call_srcu(struct srcu_struct *ssp, struct rcu_head *rhp,
			     rcu_callback_t func)
{
	call_rcu(rhp, func);
}

static inline int init_srcu_struct(struct srcu_struct *ssp)
{
	return 0;
}

DEFINE_LOCK_GUARD_1(srcu, struct srcu_struct,
		    _T->idx = srcu_read_lock(_T->lock),
		    srcu_read_unlock(_T->lock, _T->idx),
		    int idx)

#endif /* __TOOLS_LINUX_SRCU_H */
