
/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _LINUX_PERCPU_RWSEM_H
#define _LINUX_PERCPU_RWSEM_H

#include <pthread.h>
#include <linux/cleanup.h>
#include <linux/preempt.h>

struct percpu_rw_semaphore {
	pthread_mutex_t		lock;
};

static inline void percpu_down_read_preempt_disable(struct percpu_rw_semaphore *sem)
{
	pthread_mutex_lock(&sem->lock);
}

static inline void percpu_down_read(struct percpu_rw_semaphore *sem)
{
	pthread_mutex_lock(&sem->lock);
}

static inline int percpu_down_read_trylock(struct percpu_rw_semaphore *sem)
{
	return !pthread_mutex_trylock(&sem->lock);
}

static inline void percpu_up_read_preempt_enable(struct percpu_rw_semaphore *sem)
{
	pthread_mutex_unlock(&sem->lock);
}

static inline void percpu_up_read(struct percpu_rw_semaphore *sem)
{
	pthread_mutex_unlock(&sem->lock);
}

static inline void percpu_down_write(struct percpu_rw_semaphore *sem)
{
	pthread_mutex_lock(&sem->lock);
}

static inline void percpu_up_write(struct percpu_rw_semaphore *sem)
{
	pthread_mutex_unlock(&sem->lock);
}

static inline void percpu_free_rwsem(struct percpu_rw_semaphore *sem) {}

static inline int percpu_init_rwsem(struct percpu_rw_semaphore *sem)
{
	pthread_mutex_init(&sem->lock, NULL);
	return 0;
}

#define percpu_rwsem_assert_held(sem)		do {} while (0)

DEFINE_GUARD(percpu_read, struct percpu_rw_semaphore *,
	     percpu_down_read(_T), percpu_up_read(_T))
DEFINE_GUARD_COND(percpu_read, _try, percpu_down_read_trylock(_T))

DEFINE_GUARD(percpu_write, struct percpu_rw_semaphore *,
	     percpu_down_write(_T), percpu_up_write(_T))

#endif
