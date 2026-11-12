#ifndef __TOOLS_LINUX_SPINLOCK_H
#define __TOOLS_LINUX_SPINLOCK_H

#include <linux/atomic.h>
#include <linux/bug.h>
#include <linux/futex.h>
#include <urcu/futex.h>

/*
 * A futex word, not a pthread mutex: an unlocked one may be moved, as a
 * kernel spinlock may, where a copy of a pthread mutex isn't one.
 */
typedef struct {
	int32_t state;
} raw_spinlock_t;

#define SPIN_UNLOCKED	0
#define SPIN_LOCKED	1
#define SPIN_CONTENDED	2

#define __RAW_SPIN_LOCK_UNLOCKED(name)	(raw_spinlock_t) { .state = SPIN_UNLOCKED }

static inline void raw_spin_lock_init(raw_spinlock_t *lock)
{
	lock->state = SPIN_UNLOCKED;
}

static inline bool raw_spin_trylock(raw_spinlock_t *lock)
{
	int32_t old = SPIN_UNLOCKED;

	return __atomic_compare_exchange_n(&lock->state, &old, SPIN_LOCKED, false,
					   __ATOMIC_ACQUIRE, __ATOMIC_RELAXED);
}

static inline void raw_spin_lock(raw_spinlock_t *lock)
{
	if (raw_spin_trylock(lock))
		return;

	while (__atomic_exchange_n(&lock->state, SPIN_CONTENDED, __ATOMIC_ACQUIRE) != SPIN_UNLOCKED)
		futex(&lock->state, FUTEX_WAIT|FUTEX_PRIVATE_FLAG, SPIN_CONTENDED, NULL, NULL, 0);
}

static inline void raw_spin_unlock(raw_spinlock_t *lock)
{
	if (__atomic_exchange_n(&lock->state, SPIN_UNLOCKED, __ATOMIC_RELEASE) == SPIN_CONTENDED)
		futex(&lock->state, FUTEX_WAKE|FUTEX_PRIVATE_FLAG, 1, NULL, NULL, 0);
}

#define raw_spin_lock_irq(lock)		raw_spin_lock(lock)
#define raw_spin_unlock_irq(lock)	raw_spin_unlock(lock)

#define raw_spin_lock_irqsave(lock, flags)		\
do {							\
	flags = 0;					\
	raw_spin_lock(lock);				\
} while (0)

#define raw_spin_unlock_irqrestore(lock, flags) raw_spin_unlock(lock)

typedef raw_spinlock_t spinlock_t;

#define __SPIN_LOCK_UNLOCKED(name)	__RAW_SPIN_LOCK_UNLOCKED(name)

#define DEFINE_SPINLOCK(x)	spinlock_t x = __SPIN_LOCK_UNLOCKED(x)

#define spin_lock_init(lock)		raw_spin_lock_init(lock)
#define spin_lock(lock)			raw_spin_lock(lock)
#define spin_unlock(lock)		raw_spin_unlock(lock)

#define spin_lock_nested(lock, n)	spin_lock(lock)

#define spin_lock_bh(lock)		raw_spin_lock(lock)
#define spin_unlock_bh(lock)		raw_spin_unlock(lock)

#define spin_lock_irq(lock)		raw_spin_lock(lock)
#define spin_unlock_irq(lock)		raw_spin_unlock(lock)

#define spin_lock_irqsave(lock, flags)	raw_spin_lock_irqsave(lock, flags)
#define spin_lock_irqsave_nested(lock, flags, n) spin_lock_irqsave(lock, flags)
#define spin_unlock_irqrestore(lock, flags) raw_spin_unlock_irqrestore(lock, flags)
#define spin_trylock(lock)		raw_spin_trylock(lock)

#endif /* __TOOLS_LINUX_SPINLOCK_H */
