/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_UTIL_RCU_PENDING_TYPES_H
#define _BCACHEFS_UTIL_RCU_PENDING_TYPES_H

struct rcu_pending;

typedef void (*rcu_pending_process_fn)(struct rcu_pending *, struct rcu_head *);

struct rcu_pending_pcpu;

struct rcu_pending {
	struct rcu_pending_pcpu __percpu *p;
	struct srcu_struct		*srcu;
	rcu_pending_process_fn		process;
};

#endif /* _BCACHEFS_UTIL_RCU_PENDING_TYPES_H */
