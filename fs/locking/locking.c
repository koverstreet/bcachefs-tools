// SPDX-License-Identifier: GPL-2.0

/*
 * The C that Rust's locks call (locking/): what C has as a macro - lockdep's
 * classes and asserts, PF_MEMALLOC_NOIO - or as a static inline in one build
 * or the other.
 *
 * A lock initialized from Rust has a lockdep class per call site, as C's
 * init macros give theirs: Rust passes its own, @key.
 *
 * These are real (out-of-line) functions, not static inlines: both the fs and
 * bch_bindgen bindgen passes see locking.h, and a static inline would have
 * each emit its own wrap_static_fns wrapper for the same symbol - a duplicate
 * at link. A plain declaration binds to one shared definition.
 */

#include "bcachefs.h"
#include "locking/locking.h"

/* noio.rs: the NOIO scope */

unsigned int rust_memalloc_noio_save(void)
{
	return memalloc_flags_save(PF_MEMALLOC_NOIO);
}

void rust_memalloc_flags_restore(unsigned int flags)
{
	memalloc_flags_restore(flags);
}

/* raw.rs: struct mutex */

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

/* raw.rs: rw_semaphore */

void rust_init_rwsem(struct rw_semaphore *sem, const char *name, void *key)
{
#ifdef __KERNEL__
	__init_rwsem(sem, name, key);
#else
	init_rwsem(sem);
#endif
}

void rust_rwsem_assert_held(struct rw_semaphore *sem)
{
	lockdep_assert_held(sem);
}

void rust_rwsem_assert_held_write(struct rw_semaphore *sem)
{
	lockdep_assert_held_write(sem);
}

/* raw.rs: percpu_rw_semaphore */

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

void rust_percpu_rwsem_assert_held(struct percpu_rw_semaphore *sem)
{
	percpu_rwsem_assert_held(sem);
}

void rust_percpu_rwsem_assert_held_write(struct percpu_rw_semaphore *sem)
{
	lockdep_assert_held_write(sem);
}
