/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_LOCKING_H
#define _BCACHEFS_LOCKING_H

/*
 * bcachefs locking primitives that bundle a lock with the memory-reclaim
 * context it implies. See util/locking.rs for the Rust counterparts.
 */

#include <linux/cleanup.h>
#include <linux/jiffies.h>
#include <linux/mutex.h>
#include <linux/percpu-rwsem.h>
#include <linux/sched.h>
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
 *
 * Owner tracking: a mutex_noio is by construction taken across allocating work,
 * which makes these the locks that go long precisely when the machine is short
 * on memory - and the ones a stall report can't name a holder for. Recording
 * the acquire site, the holder and when it was taken costs three stores, and
 * turns "everything is waiting, on what?" into a line the log already carries.
 * See bch2_mutex_noio_to_text().
 *
 * held_from doubles as the held flag, since an acquire site is never address 0,
 * and is published last and cleared first so a reader that tests it before
 * reading the rest mostly sees a coherent set. A pid can't serve that purpose:
 * 0 is a value it legitimately takes.
 *
 * Best effort, and diagnostics only: read without the lock, so a dump racing an
 * unlock can name a holder that has just left. Nothing decides on these fields.
 */
struct mutex_noio {
	struct mutex	lock;
	unsigned long	held_from;
	unsigned long	held_at;
	pid_t		held_by;
};

static inline void mutex_noio_init(struct mutex_noio *m)
{
	mutex_init(&m->lock);
	m->held_from	= 0;
	m->held_at	= 0;
	m->held_by	= 0;
}

static inline void __mutex_noio_set_owner(struct mutex_noio *m, unsigned long ip)
{
	WRITE_ONCE(m->held_at,		jiffies);
	WRITE_ONCE(m->held_by,		current->pid);
	smp_store_release(&m->held_from, ip);
}

static inline void __mutex_noio_clear_owner(struct mutex_noio *m)
{
	WRITE_ONCE(m->held_from, 0);
}

DEFINE_LOCK_GUARD_1(mutex_noio, struct mutex_noio,
		    _T->flags = memalloc_flags_save(PF_MEMALLOC_NOIO);
		    mutex_lock(&_T->lock->lock);
		    __mutex_noio_set_owner(_T->lock, _THIS_IP_),
		    __mutex_noio_clear_owner(_T->lock);
		    mutex_unlock(&_T->lock->lock);
		    memalloc_flags_restore(_T->flags),
		    unsigned int flags)

struct printbuf;
void bch2_mutex_noio_to_text(struct printbuf *, struct mutex_noio *);

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
