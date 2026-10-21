// SPDX-License-Identifier: GPL-2.0

/*
 * The C that util/rcu.rs calls: srcu_read_lock() and _unlock() are static
 * inlines whose kernel Rust helpers are newer than the oldest kernel we build
 * against. Out of line, as locking/locking.c's are, for the same reason.
 */

#include "bcachefs.h"
#include "util/rcu_gen.h"

int rust_srcu_read_lock(struct srcu_struct *ssp)
{
	return srcu_read_lock(ssp);
}

void rust_srcu_read_unlock(struct srcu_struct *ssp, int idx)
{
	srcu_read_unlock(ssp, idx);
}
