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

/*
 * mutex_noio - a mutex that also establishes a PF_MEMALLOC_NOIO scope while
 * held.
 *
 * Many bcachefs mutexes - sb_lock above all - are taken precisely to guard
 * allocations that must not recurse into reclaim IO: a filesystem that drives
 * the block layer directly can't let reclaim loop back through the device it's
 * allocating for. Pairing every such lock with a separate
 * guard(memalloc_flags)(PF_MEMALLOC_NOIO) is easy to forget (and was, on many
 * sb_lock sites). Folding the NOIO scope into the lock type makes it a property
 * of the lock: holding it _is_ the NOIO context, and you can't take it without.
 *
 * Guard-only by design. The saved memalloc flags live in the guard object, so a
 * raw lock/unlock pair would have nowhere to stash them; scoped use also
 * guarantees the LIFO nesting that memalloc_flags_save/restore require. Use
 * guard(mutex_noio)(&m) or scoped_guard(mutex_noio, &m).
 */
struct mutex_noio {
	struct mutex	lock;
};

static inline void mutex_noio_init(struct mutex_noio *m)
{
	mutex_init(&m->lock);
}

DEFINE_LOCK_GUARD_1(mutex_noio, struct mutex_noio,
		    _T->flags = memalloc_flags_save(PF_MEMALLOC_NOIO); mutex_lock(&_T->lock->lock),
		    mutex_unlock(&_T->lock->lock); memalloc_flags_restore(_T->flags),
		    unsigned int flags)

/*
 * percpu_rwsem_noio - a percpu_rwsem that establishes a PF_MEMALLOC_NOIO scope
 * while held, the percpu_rwsem analogue of mutex_noio. Used for rwsems like
 * capacity.mark_lock that are taken over allocating work. Guards mirror the
 * kernel's percpu_read/percpu_write, with _noio.
 *
 * A few hot paths take the lock raw (percpu_down_read on the inner
 * percpu_rw_semaphore) rather than via the guard - that's sound only where the
 * caller is already in a NOIO context (e.g. holding a locked btree_trans);
 * such sites reach through .lock with a comment saying why.
 */
struct percpu_rwsem_noio {
	struct percpu_rw_semaphore	lock;
};

DEFINE_LOCK_GUARD_1(percpu_read_noio, struct percpu_rwsem_noio,
		    _T->flags = memalloc_flags_save(PF_MEMALLOC_NOIO); percpu_down_read(&_T->lock->lock),
		    percpu_up_read(&_T->lock->lock); memalloc_flags_restore(_T->flags),
		    unsigned int flags)

DEFINE_LOCK_GUARD_1(percpu_write_noio, struct percpu_rwsem_noio,
		    _T->flags = memalloc_flags_save(PF_MEMALLOC_NOIO); percpu_down_write(&_T->lock->lock),
		    percpu_up_write(&_T->lock->lock); memalloc_flags_restore(_T->flags),
		    unsigned int flags)

/*
 * Bindgen shims for the Rust memalloc guards (util/locking.rs).
 * memalloc_flags_save/restore are kernel static inlines outside bcachefs, and
 * PF_MEMALLOC_NOIO is a bare #define that doesn't reach Rust; wrap them under
 * bcachefs-owned rust_* names so the flag stays on the C side. Save is
 * per-flag; restore just replays saved flags.
 *
 * These are real (out-of-line) functions, defined in util/locking.c, not static
 * inlines: both the fs and bch_bindgen bindgen passes see this header, and a
 * static inline would have each emit its own wrap_static_fns wrapper for the
 * same symbol - a duplicate at link. A plain declaration binds to one shared
 * definition.
 */
unsigned int rust_memalloc_noio_save(void);
void rust_memalloc_flags_restore(unsigned int flags);

#endif /* _BCACHEFS_LOCKING_H */
