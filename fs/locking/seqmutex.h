/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_SEQMUTEX_H
#define _BCACHEFS_SEQMUTEX_H

#include <linux/mutex.h>

#include "locking/seqmutex_gen.h"

#define seqmutex_init(_lock)	mutex_init(&(_lock)->lock)

static inline void seqmutex_lock(struct seqmutex *lock)
{
	mutex_lock(&lock->lock);
	lock->seq++;
}

static inline u32 seqmutex_unlock(struct seqmutex *lock)
{
	u32 seq = lock->seq;
	mutex_unlock(&lock->lock);
	return seq;
}

#endif /* _BCACHEFS_SEQMUTEX_H */
