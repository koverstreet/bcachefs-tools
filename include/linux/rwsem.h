#ifndef __TOOLS_LINUX_RWSEM_H
#define __TOOLS_LINUX_RWSEM_H

#include <pthread.h>
#include <linux/cleanup.h>

struct rw_semaphore {
	pthread_rwlock_t	lock;
};

#define __RWSEM_INITIALIZER(name)				\
	{ .lock = PTHREAD_RWLOCK_INITIALIZER }

#define DECLARE_RWSEM(name) \
	struct rw_semaphore name = __RWSEM_INITIALIZER(name)

static inline void init_rwsem(struct rw_semaphore *lock)
{
	pthread_rwlock_init(&lock->lock, NULL);
}

static inline void down_read(struct rw_semaphore *sem)
{
	pthread_rwlock_rdlock(&sem->lock);
}

static inline int down_read_trylock(struct rw_semaphore *sem)
{
	return !pthread_rwlock_tryrdlock(&sem->lock);
}

static inline int down_read_interruptible(struct rw_semaphore *sem)
{
	pthread_rwlock_rdlock(&sem->lock);
	return 0;
}

static inline int down_read_killable(struct rw_semaphore *sem)
{
	pthread_rwlock_rdlock(&sem->lock);
	return 0;
}

static inline void up_read(struct rw_semaphore *sem)
{
	pthread_rwlock_unlock(&sem->lock);
}

static inline void down_write(struct rw_semaphore *sem)
{
	pthread_rwlock_wrlock(&sem->lock);
}

static inline int down_write_trylock(struct rw_semaphore *sem)
{
	return !pthread_rwlock_trywrlock(&sem->lock);
}

static inline void up_write(struct rw_semaphore *sem)
{
	pthread_rwlock_unlock(&sem->lock);
}

DEFINE_GUARD(rwsem_read, struct rw_semaphore *, down_read(_T), up_read(_T))
DEFINE_GUARD_COND(rwsem_read, _try, down_read_trylock(_T))
DEFINE_GUARD_COND(rwsem_read, _intr, down_read_interruptible(_T) == 0)

DEFINE_GUARD(rwsem_write, struct rw_semaphore *, down_write(_T), up_write(_T))
DEFINE_GUARD_COND(rwsem_write, _try, down_write_trylock(_T))

#endif /* __TOOLS_LINUX_RWSEM_H */
