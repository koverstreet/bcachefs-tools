/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_UTIL_LOCKING_TYPES_H
#define _BCACHEFS_UTIL_LOCKING_TYPES_H

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

#endif /* _BCACHEFS_UTIL_LOCKING_TYPES_H */
