// SPDX-License-Identifier: GPL-2.0

/*
 * The C that util/rcu.rs calls: init_srcu_struct() is a macro under lockdep,
 * and the rest static inlines in userspace, whose kernel Rust helpers are
 * newer than the oldest kernel we build against. Out of line, as
 * locking/locking.c's are, for the same reason.
 */

#include "bcachefs.h"
#include "util/rcu_gen.h"

int rust_init_srcu_struct(struct srcu_struct *ssp, const char *name, void *key)
{
#ifdef CONFIG_DEBUG_LOCK_ALLOC
	return __init_srcu_struct(ssp, name, key);
#else
	return init_srcu_struct(ssp);
#endif
}

void rust_synchronize_srcu_expedited(struct srcu_struct *ssp)
{
	synchronize_srcu_expedited(ssp);
}

void rust_cleanup_srcu_struct(struct srcu_struct *ssp)
{
	cleanup_srcu_struct(ssp);
}

int rust_srcu_read_lock(struct srcu_struct *ssp)
{
	return srcu_read_lock(ssp);
}

void rust_srcu_read_unlock(struct srcu_struct *ssp, int idx)
{
	srcu_read_unlock(ssp, idx);
}
