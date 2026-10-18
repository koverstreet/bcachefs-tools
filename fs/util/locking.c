// SPDX-License-Identifier: GPL-2.0

/*
 * Bindgen shims for the Rust memalloc guards (util/locking.rs).
 * memalloc_flags_save/restore are kernel static inlines outside bcachefs, and
 * PF_MEMALLOC_NOIO is a bare #define that doesn't reach Rust; wrap them under
 * bcachefs-owned rust_* names so the flag stays on the C side. Save is
 * per-flag; restore just replays saved flags.
 *
 * These are real (out-of-line) functions, not static inlines: both the fs and
 * bch_bindgen bindgen passes see util/locking.h, and a static inline would
 * have each emit its own wrap_static_fns wrapper for the same symbol - a
 * duplicate at link. A plain declaration binds to one shared definition.
 */

#include "bcachefs.h"
#include "util/locking.h"

unsigned int rust_memalloc_noio_save(void)
{
	return memalloc_flags_save(PF_MEMALLOC_NOIO);
}

void rust_memalloc_flags_restore(unsigned int flags)
{
	memalloc_flags_restore(flags);
}

/*
 * For PercpuRwSem (util/sync.rs): percpu_init_rwsem() is a macro, with a
 * lockdep class per call site - Rust passes its own, @key - and the rest are
 * static inlines in one build or the other.
 */

int rust_percpu_init_rwsem(struct percpu_rw_semaphore *sem, const char *name, void *key)
{
#ifdef __KERNEL__
	return __percpu_init_rwsem(sem, name, key);
#else
	return percpu_init_rwsem(sem);
#endif
}

void rust_percpu_free_rwsem(struct percpu_rw_semaphore *sem)
{
	percpu_free_rwsem(sem);
}

void rust_percpu_down_read(struct percpu_rw_semaphore *sem)
{
	percpu_down_read(sem);
}

void rust_percpu_up_read(struct percpu_rw_semaphore *sem)
{
	percpu_up_read(sem);
}

void rust_percpu_down_write(struct percpu_rw_semaphore *sem)
{
	percpu_down_write(sem);
}

void rust_percpu_up_write(struct percpu_rw_semaphore *sem)
{
	percpu_up_write(sem);
}

/*
 * For MutexNoio (util/locking.rs): mutex_init() is a macro, with a lockdep
 * class per call site - Rust passes its own, @key - and lockdep_assert_held()
 * is one too.
 */

void rust_mutex_init(struct mutex *lock, const char *name, void *key)
{
#ifdef __KERNEL__
	__mutex_init(lock, name, key);
#else
	mutex_init(lock);
#endif
}

void rust_mutex_assert_held(struct mutex *lock)
{
	lockdep_assert_held(lock);
}

/*
 * For SrcuReadGuard: static inlines, whose kernel Rust helpers are newer than
 * the oldest kernel we build against.
 */

int rust_srcu_read_lock(struct srcu_struct *ssp)
{
	return srcu_read_lock(ssp);
}

void rust_srcu_read_unlock(struct srcu_struct *ssp, int idx)
{
	srcu_read_unlock(ssp, idx);
}
