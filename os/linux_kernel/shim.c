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

/* os/linux_kernel/spinlock.rs: spinlock_t, as the kernel's Rust helper initializes it */

void rust_spin_lock_init(spinlock_t *lock, const char *name, void *key)
{
#ifdef CONFIG_DEBUG_SPINLOCK
# ifdef CONFIG_PREEMPT_RT
	__spin_lock_init(lock, name, key, false);
# else
	__raw_spin_lock_init(spinlock_check(lock), name, key, LD_WAIT_CONFIG);
# endif
#else
	spin_lock_init(lock);
#endif
}

unsigned long rust_spin_lock_irqsave(spinlock_t *lock)
{
	unsigned long flags;

	spin_lock_irqsave(lock, flags);
	return flags;
}

unsigned long rust_spin_lock_irqsave_nested(spinlock_t *lock, unsigned subclass)
{
	unsigned long flags;

	spin_lock_irqsave_nested(lock, flags, subclass);
	return flags;
}

void rust_spin_unlock_irqrestore(spinlock_t *lock, unsigned long flags)
{
	spin_unlock_irqrestore(lock, flags);
}

/* os/linux_kernel/spinlock.rs: raw_spinlock_t */

void rust_raw_spin_lock_init(raw_spinlock_t *lock, const char *name, void *key)
{
#ifdef CONFIG_DEBUG_SPINLOCK
	__raw_spin_lock_init(lock, name, key, LD_WAIT_SPIN);
#else
	raw_spin_lock_init(lock);
#endif
}

void rust_raw_spin_lock(raw_spinlock_t *lock)
{
	raw_spin_lock(lock);
}

void rust_raw_spin_unlock(raw_spinlock_t *lock)
{
	raw_spin_unlock(lock);
}

/* os/linux_kernel/rwsem.rs: rw_semaphore */

void rust_init_rwsem(struct rw_semaphore *sem, const char *name, void *key)
{
	__init_rwsem(sem, name, key);
}
