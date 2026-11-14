// SPDX-License-Identifier: GPL-2.0

/*
 * The C that Rust's locks call (locking/): what C has as a macro - lockdep's
 * classes and asserts, spin_lock_irqsave() - or as a static inline in one
 * build or the other.
 *
 * A lock initialized from Rust has a lockdep class per call site, as C's
 * init macros give theirs: Rust passes its own, @key.
 *
 * These are real (out-of-line) functions, not static inlines: both the fs and
 * bch_bindgen bindgen passes see locking.h, and a static inline would have
 * each emit its own wrap_static_fns wrapper for the same symbol - a duplicate
 * at link. A plain declaration binds to one shared definition.
 */

#include <linux/irqflags.h>
#include <linux/local_lock.h>
#include <linux/mutex.h>
#include <linux/percpu-rwsem.h>
#include <linux/rwsem.h>
#include <linux/sched/mm.h>
#include <linux/spinlock.h>

#include <trace/events/lock.h>

#include "locking/shim.h"

/* raw.rs: struct mutex */

void rust_mutex_init(struct mutex *lock, const char *name, void *key)
{
#ifdef __KERNEL__
	__mutex_init(lock, name, key);
#else
	mutex_init(lock);
#endif
}

/* raw.rs: spinlock_t, as the kernel's Rust helper initializes it */

void rust_spin_lock_init(spinlock_t *lock, const char *name, void *key)
{
#if defined(__KERNEL__) && defined(CONFIG_DEBUG_SPINLOCK)
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

/* raw.rs: raw_spinlock_t */

void rust_raw_spin_lock_init(raw_spinlock_t *lock, const char *name, void *key)
{
#if defined(__KERNEL__) && defined(CONFIG_DEBUG_SPINLOCK)
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

/* raw.rs: the contention tracepoints, for a lock whose state is ours */

void rust_trace_contention_begin(const void *lock, unsigned int flags)
{
	trace_contention_begin((void *) lock, flags);
}

void rust_trace_contention_end(const void *lock, int ret)
{
	trace_contention_end((void *) lock, ret);
}

/*
 * percpu.rs: local_lock_t - one cpu's copy initialized, and this cpu's taken
 * irqsave, by its percpu pointer, as the util.c percpu helpers take theirs
 */

void rust_local_lock_init(local_lock_t *lock)
{
	local_lock_init(lock);
}

unsigned long rust_local_lock_irqsave(local_lock_t *pcp)
{
	unsigned long flags;

	local_lock_irqsave((local_lock_t __percpu *) (unsigned long) pcp, flags);
	return flags;
}

void rust_local_unlock_irqrestore(local_lock_t *pcp, unsigned long flags)
{
	local_unlock_irqrestore((local_lock_t __percpu *) (unsigned long) pcp, flags);
}

/* percpu.rs: IrqGuard - this cpu's interrupts off, for what they change too */

unsigned long rust_local_irq_save(void)
{
	unsigned long flags;

	local_irq_save(flags);
	return flags;
}

void rust_local_irq_restore(unsigned long flags)
{
	local_irq_restore(flags);
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
