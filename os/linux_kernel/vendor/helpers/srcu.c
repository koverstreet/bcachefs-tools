// SPDX-License-Identifier: GPL-2.0

#include <linux/srcu.h>
#include <linux/version.h>

__rust_helper int rust_helper_init_srcu_struct_with_key(struct srcu_struct *ssp,
							const char *name,
							struct lock_class_key *key)
{
#if LINUX_VERSION_CODE >= KERNEL_VERSION(7, 3, 0) || defined(CONFIG_DEBUG_LOCK_ALLOC)
	return __init_srcu_struct(ssp, name, key);
#else
	/*
	 * Before 7.3, __init_srcu_struct() is lockdep's: without
	 * CONFIG_DEBUG_LOCK_ALLOC, init_srcu_struct() is the function, and has
	 * no use for the name or key.
	 */
	return init_srcu_struct(ssp);
#endif
}

__rust_helper bool rust_helper_srcu_readers_active(struct srcu_struct *ssp)
{
#if LINUX_VERSION_CODE >= KERNEL_VERSION(7, 3, 0)
	return srcu_readers_active(ssp);
#else
	/*
	 * Before 7.3 srcu_readers_active() is srcutree.c's own. Srcu's drop asks
	 * only to warn of a leaked guard and wait it out; cleanup_srcu_struct()
	 * still WARNs of live readers itself.
	 */
	return false;
#endif
}

__rust_helper int rust_helper_srcu_read_lock(struct srcu_struct *ssp)
{
	return srcu_read_lock(ssp);
}

__rust_helper void rust_helper_srcu_read_unlock(struct srcu_struct *ssp, int idx)
{
	srcu_read_unlock(ssp, idx);
}

__rust_helper void rust_helper_srcu_barrier(struct srcu_struct *ssp)
{
	srcu_barrier(ssp);
}

__rust_helper void rust_helper_synchronize_srcu_expedited(struct srcu_struct *ssp)
{
	synchronize_srcu_expedited(ssp);
}
