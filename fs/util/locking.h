/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_LOCKING_H
#define _BCACHEFS_LOCKING_H

/*
 * bcachefs locking primitives that bundle a lock with the memory-reclaim
 * context it implies. See util/locking.rs for the Rust counterparts.
 */

#include <linux/cleanup.h>
#include <linux/mutex.h>
#include <linux/percpu-rwsem.h>
#include <linux/sched/mm.h>

#include "util/locking_gen.h"

static inline void mutex_noio_init(struct mutex_noio *m)
{
	mutex_init(&m->lock);
}

DEFINE_LOCK_GUARD_1(mutex_noio, struct mutex_noio,
		    _T->flags = memalloc_flags_save(PF_MEMALLOC_NOIO); mutex_lock(&_T->lock->lock),
		    mutex_unlock(&_T->lock->lock); memalloc_flags_restore(_T->flags),
		    unsigned int flags)

DEFINE_LOCK_GUARD_1(percpu_read_noio, struct percpu_rwsem_noio,
		    _T->flags = memalloc_flags_save(PF_MEMALLOC_NOIO); percpu_down_read(&_T->lock->lock),
		    percpu_up_read(&_T->lock->lock); memalloc_flags_restore(_T->flags),
		    unsigned int flags)

DEFINE_LOCK_GUARD_1(percpu_write_noio, struct percpu_rwsem_noio,
		    _T->flags = memalloc_flags_save(PF_MEMALLOC_NOIO); percpu_down_write(&_T->lock->lock),
		    percpu_up_write(&_T->lock->lock); memalloc_flags_restore(_T->flags),
		    unsigned int flags)

#endif /* _BCACHEFS_LOCKING_H */
