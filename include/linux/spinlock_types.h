#ifndef __TOOLS_LINUX_SPINLOCK_H
#define __TOOLS_LINUX_SPINLOCK_H

#include <linux/atomic.h>
#include <linux/bug.h>
#include <pthread.h>

typedef struct {
	pthread_mutex_t lock;
} raw_spinlock_t;

#define __RAW_SPIN_LOCK_UNLOCKED(name)	(raw_spinlock_t) { .lock = PTHREAD_MUTEX_INITIALIZER  }

static inline void raw_spin_lock_init(raw_spinlock_t *lock)
{
	pthread_mutex_init(&lock->lock, NULL);
}

static inline bool raw_spin_trylock(raw_spinlock_t *lock)
{
	return !pthread_mutex_trylock(&lock->lock);
}

static inline void raw_spin_lock(raw_spinlock_t *lock)
{
	pthread_mutex_lock(&lock->lock);
}

static inline void raw_spin_unlock(raw_spinlock_t *lock)
{
	pthread_mutex_unlock(&lock->lock);
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
#define spin_unlock_irqrestore(lock, flags) raw_spin_unlock_irqrestore(lock, flags)
#define spin_trylock(lock)		raw_spin_trylock(lock)

#endif /* __TOOLS_LINUX_SPINLOCK_H */
