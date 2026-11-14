// SPDX-License-Identifier: GPL-2.0

/*
 * The C that the kernel port's Rust calls: what C has as a macro - lockdep's
 * classes and asserts, spin_lock_irqsave() - or as a static inline.
 *
 * A lock initialized from Rust has a lockdep class per call site, as C's
 * init macros give theirs: Rust passes its own, @key.
 */

#include <linux/mutex.h>
#include <linux/percpu-rwsem.h>
#include <linux/rwsem.h>
#include <linux/spinlock.h>

#include "shim.h"

/* os/linux_kernel/lockdep.rs: the C locks' asserts */

void rust_mutex_assert_held(struct mutex *lock)
{
	lockdep_assert_held(lock);
}

void rust_spin_lock_assert_held(spinlock_t *lock)
{
	lockdep_assert_held(lock);
}

void rust_raw_spin_lock_assert_held(raw_spinlock_t *lock)
{
	lockdep_assert_held(lock);
}

void rust_rwsem_assert_held(struct rw_semaphore *sem)
{
	lockdep_assert_held(sem);
}

void rust_rwsem_assert_held_write(struct rw_semaphore *sem)
{
	lockdep_assert_held_write(sem);
}

void rust_percpu_rwsem_assert_held(struct percpu_rw_semaphore *sem)
{
	percpu_rwsem_assert_held(sem);
}

void rust_percpu_rwsem_assert_held_write(struct percpu_rw_semaphore *sem)
{
	lockdep_assert_held_write(sem);
}

/* a lock whose state is Rust's - six - by its map: the macro wants the lock */
void rust_lockdep_set_notrack_class(struct lockdep_map *map)
{
	lockdep_set_notrack_class((struct { struct lockdep_map dep_map; } *) map);
}

/* os/linux_kernel/lockdep.rs: lockdep's hooks, for a lock whose state is ours */

#ifdef CONFIG_DEBUG_LOCK_ALLOC
void rust_lockdep_init_map(void *map, const char *name, void *key)
{
	lockdep_init_map(map, name, key, 0);
}

void rust_lock_acquire(void *map, int trylock, int read, unsigned long ip)
{
	lock_acquire(map, 0, trylock, read, 1, NULL, ip);
}

void rust_lock_release(void *map, unsigned long ip)
{
	lock_release(map, ip);
}

void rust_lock_contended(void *map, unsigned long ip)
{
	lock_contended(map, ip);
}

void rust_lock_acquired(void *map, unsigned long ip)
{
	lock_acquired(map, ip);
}

void rust_debug_check_no_locks_freed(const void *p, unsigned long len)
{
	debug_check_no_locks_freed(p, len);
}
#endif
