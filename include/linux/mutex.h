#ifndef __TOOLS_LINUX_MUTEX_H
#define __TOOLS_LINUX_MUTEX_H

#include <pthread.h>

struct mutex {
	pthread_mutex_t lock;
};

#define DEFINE_MUTEX(mutexname) \
	struct mutex mutexname = { .lock = PTHREAD_MUTEX_INITIALIZER }

static inline void mutex_init(struct mutex *l)
{
	pthread_mutex_init(&l->lock, NULL);
}

static inline void mutex_lock(struct mutex *l)
{
	pthread_mutex_lock(&l->lock);
}

static inline bool mutex_trylock(struct mutex *l)
{
	return !pthread_mutex_trylock(&l->lock);
}

static inline void mutex_unlock(struct mutex *l)
{
	pthread_mutex_unlock(&l->lock);
}

DEFINE_GUARD(mutex, struct mutex *, mutex_lock(_T), mutex_unlock(_T))

#endif /* __TOOLS_LINUX_MUTEX_H */
