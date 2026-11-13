// SPDX-License-Identifier: GPL-2.0

/*
 * The C that util/rcu.rs calls: init_srcu_struct() is a macro under lockdep.
 */

#include "bcachefs.h"
#include "util/shim.h"

int rust_init_srcu_struct(struct srcu_struct *ssp, const char *name, void *key)
{
#ifdef CONFIG_DEBUG_LOCK_ALLOC
	return __init_srcu_struct(ssp, name, key);
#else
	return init_srcu_struct(ssp);
#endif
}
