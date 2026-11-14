// SPDX-License-Identifier: GPL-2.0

/* DOC(btree-key-cache)
 *
 * Fast single-key lookups for btrees where the same keys are read and updated
 * frequently. The primary user is the alloc btree: extent updates need to
 * update allocation information for the affected buckets, and doing a full
 * btree lookup for each would be expensive. The key cache keeps recently
 * accessed alloc keys in a hash table, allowing updates to be applied directly
 * without a btree traversal. Cached entries are flushed to the btree by
 * journal reclaim.
 */

mod fill;
mod flush;
pub(crate) use flush::{bch2_btree_insert_key_cached, bch2_btree_key_cache_drop};

use crate::btree::bkey::{BkeySC, SPOS_MAX};
use crate::btree::bkey_buf::copy_key;
use crate::btree::locking::bch2_btree_lock_init;
use crate::btree::iter::TransAttempt;
use crate::btree::path::Paths;
use crate::btree::trans::BtreeTrans;
use crate::c;
use crate::data_structures::cuckoo::{Cuckoo, Entry};
use crate::errcode::{Errcode, BchError};
use crate::format::{Bpos, Btree};
use crate::fs::Fs;
use crate::locking::six::{six_lock_exit, SixHold};
use crate::prt;
use crate::util::alloc::{current_gfp_context_raw, init_at, krealloc};
use crate::util::hash::GoldenRatio;
use crate::os::kernel::local_clock;
use crate::util::kmem_cache::KmemCache;
use crate::util::place::{Contained, PlacedDrop};
use crate::util::rcu::{read_lock, ReadGuard, SrcuReadGuard};
use crate::util::rcu_pending::{Mode, RcuPending};
use crate::os::shrinker::{Shrink, Shrinker};
use crate::os::workqueue::Queue;
use crate::util::time_stats::TimeStats;
use crate::util::{Printbuf, ToText};
use c::SixLockType::{SIX_LOCK_intent, SIX_LOCK_write};
#[cfg(kernel)]
use core::ffi::c_int;
use core::ffi::c_ulong;
use core::mem::{offset_of, ManuallyDrop};
use core::ops::{Deref, DerefMut};
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicUsize, Ordering::Relaxed};

pub(crate) type KeyTable = Cuckoo<Entry<c::bkey_cached>, GoldenRatio>;

crate::keyed_by!(c::bkey_cached, |ck| &ck.key => c::bkey_cached_key);

// what's shared is the entry lock's, or atomic
unsafe impl Sync for c::bkey_cached {}

fn btree_uses_pcpu_readers(id: Btree) -> bool {
    id == Btree::subvolumes
}

static KEY_CACHE: KmemCache<c::bkey_cached> = KmemCache::new();

/// The shrinker's counts: bumped by concurrent scans, as C's plain counters
/// were.
pub(crate) struct ShrinkStats {
    freed:             AtomicUsize,
    skipped_dirty:     AtomicUsize,
    skipped_accessed:  AtomicUsize,
    skipped_lock_fail: AtomicUsize,
}

impl ShrinkStats {
    pub(crate) const fn new() -> Self {
        Self {
            freed:             AtomicUsize::new(0),
            skipped_dirty:     AtomicUsize::new(0),
            skipped_accessed:  AtomicUsize::new(0),
            skipped_lock_fail: AtomicUsize::new(0),
        }
    }
}

fn inc(n: &AtomicUsize) {
    n.fetch_add(1, Relaxed);
}

/// What an entry freed waits out the btree trans SRCU for: its lock exited,
/// and it back to the slab.
#[derive(::pin_init::Zeroable)]
pub(crate) struct Free {}

unsafe impl Mode for Free {
    type Item = NonNull<c::bkey_cached>;

    fn into_raw(ck: Self::Item) -> NonNull<u8> {
        unsafe { NonNull::new_unchecked(&raw mut (*ck.as_ptr()).rcu).cast() }
    }

    unsafe fn from_raw(p: NonNull<u8>) -> Self::Item {
        unsafe { p.byte_sub(offset_of!(c::bkey_cached, rcu)).cast() }
    }

    unsafe fn process(&self, _pending: &RcuPending<'_, Self>, ck: Self::Item) {
        unsafe {
            six_lock_exit((&raw mut (*ck.as_ptr()).c.lock).cast());
            KEY_CACHE.free(ck);
        }
    }
}

pub(crate) type Pending = RcuPending<'static, Free>;

/// An entry of the cache's, found unlocked: for as long as the transaction
/// holds the btree trans SRCU, which it's freed a grace period of.
///
/// # Safety
/// @ck is the cache's, found under @trans's SRCU.
pub(super) unsafe fn cached<'t>(_trans: &'t BtreeTrans<'_>, ck: *const c::bkey_cached) -> &'t c::bkey_cached {
    unsafe { &*ck }
}

/// An entry, held write locked: what its plain fields are written through.
pub(super) struct LockedEntry<'a> {
    ck:   NonNull<c::bkey_cached>,
    held: SixHold<'a>,
}

impl<'a> LockedEntry<'a> {
    fn new(ck: &'a c::bkey_cached, held: SixHold<'a>) -> Self {
        assert!(held.ty() == SIX_LOCK_write && ptr::eq(held.lock(), &ck.c.lock),
                "a key cache entry's handle without its write lock");
        LockedEntry { ck: NonNull::from(ck), held }
    }

    fn into_parts(self) -> (&'a c::bkey_cached, SixHold<'a>) {
        (unsafe { self.ck.as_ref() }, self.held)
    }
}

impl Deref for LockedEntry<'_> {
    type Target = c::bkey_cached;

    fn deref(&self) -> &c::bkey_cached {
        unsafe { self.ck.as_ref() }
    }
}

impl DerefMut for LockedEntry<'_> {
    fn deref_mut(&mut self) -> &mut c::bkey_cached {
        unsafe { self.ck.as_mut() }
    }
}

fn bkey_cached_trylock(ck: &c::bkey_cached) -> Option<LockedEntry<'_>> {
    let intent = ck.c.lock.try_lock_as(SIX_LOCK_intent)?;
    Some(LockedEntry::new(ck, intent.try_write().ok()?))
}

impl BtreeTrans<'_> {
    pub(crate) fn btree_key_cache_find(&self, btree: Btree, pos: Bpos) -> Option<&c::bkey_cached> {
        let key = c::bkey_cached_key { btree_id: btree.0, pos };

        self.fs().btree.key_cache.table.get_entry(&key, &read_lock()).map(|ck| unsafe { cached(self, ck) })
    }
}

fn bkey_cached_lock_for_evict(ck: &c::bkey_cached) -> Option<LockedEntry<'_>> {
    let intent = ck.c.lock.try_lock_as(SIX_LOCK_intent)?;

    if ck.dirty() {
        return None;
    }

    Some(LockedEntry::new(ck, intent.try_write().ok()?))
}

impl c::bch_fs_btree_key_cache {
    fn bkey_cached_evict(&self, ck: &mut LockedEntry<'_>) -> bool {
        let ret = self.table.remove_entry(&ck.key, ck.ck);
        if ret {
            self.keys.remove(ck.keys_idx);
            ck.keys_idx = 0;
            ck.key = c::bkey_cached_key { btree_id: !0, pos: SPOS_MAX };
            self.nr_keys.fetch_sub(1, Relaxed);
        }

        ret
    }

    /// Its entries, from slot @idx: for as long as @srcu, the btree trans
    /// SRCU's, is held.
    fn keys_from<'r, 's: 'r>(&'s self, idx: usize, rcu: &'r ReadGuard, srcu: &'s SrcuReadGuard<'_>)
        -> impl Iterator<Item = (usize, &'s c::bkey_cached)> + 'r
    {
        ebug_assert!(ptr::eq(srcu.srcu(), &self.fs().btree.trans.barrier));
        self.keys.items_from(idx, rcu).map(|(i, ck)| (i, unsafe { ck.cast::<c::bkey_cached>().as_ref() }))
    }

    /*
     * On bc->keys from before it's in the table until it's evicted: walkers hold
     * the btree trans SRCU, and an evicted entry is freed a grace period of it
     * later - so the walks never see one that's been freed.
     */
    fn bkey_cached_keys_add(&self, ck: &mut LockedEntry<'_>) -> Result<(), BchError> {
        ck.keys_idx = self.keys.add(ck.ck.cast())
            .map_err(|_| BchError::from(Errcode::ENOMEM_btree_key_cache_create))?;
        Ok(())
    }

    fn bkey_cached_free_noassert(&self, mut ck: LockedEntry<'_>) {
        ck.free_key_buf();

        let (ck, held) = ck.into_parts();
        drop(held);

        let pcpu_readers = ck.c.lock.six().readers.allocated();
        self.pending()[pcpu_readers as usize].enqueue(NonNull::from(ck));
    }

    fn bkey_cached_free(&self, trans: &BtreeTrans<'_>, ck: LockedEntry<'_>) {
        /*
         * we'll hit strange issues in the SRCU code if we aren't holding an
         * SRCU read lock...
         */
        ebug_assert!(trans.srcu_held());

        self.bkey_cached_free_noassert(ck);
    }

    fn bkey_cached_reuse<'s>(&'s self, srcu: &'s SrcuReadGuard<'_>) -> Option<LockedEntry<'s>> {
        let rcu = read_lock();
        for (_, ck) in self.keys_from(0, &rcu, srcu) {
            if !ck.dirty() {
                if let Some(mut ck) = bkey_cached_lock_for_evict(ck) {
                    if self.bkey_cached_evict(&mut ck) {
                        return Some(ck);
                    }
                }
            }
        }
        None
    }
}

/// An entry's key buffer, of @u64s: kmalloc()'d, as C's commit path reallocs
/// and kfree()s it. Freed if it's dropped.
struct NewKey {
    k:    NonNull<u8>,
    u64s: u16,
}

impl NewKey {
    fn alloc(u64s: usize) -> Option<Self> {
        let k = unsafe { krealloc(ptr::null_mut(), u64s * size_of::<u64>()) }.ok()?;
        Some(NewKey { k, u64s: u64s as u16 })
    }
}

impl Drop for NewKey {
    fn drop(&mut self) {
        unsafe { crate::util::alloc::kfree_unsized(self.k.as_ptr().cast()) }
    }
}

/*
 * An entry's key is in a buffer of u64s u64s, kmalloc()'d - or none, null and
 * 0, its entry freed. What changes either holds the entry write locked.
 */
impl c::bkey_cached {
    fn set_key_buf(&mut self, k: NewKey) {
        let k = ManuallyDrop::new(k);
        unsafe { crate::util::alloc::kfree_unsized(self.k.cast()) };
        (self.k, self.u64s) = (k.k.as_ptr().cast(), k.u64s);
    }

    fn free_key_buf(&mut self) {
        unsafe { crate::util::alloc::kfree_unsized(self.k.cast()) };
        (self.k, self.u64s) = (ptr::null_mut(), 0);
    }

    fn set_key(&mut self, k: BkeySC<'_>) {
        assert!(u16::from(k.k.u64s) <= self.u64s, "a key cache entry given a key of {} u64s, its buffer {}",
                k.k.u64s, self.u64s);
        unsafe { copy_key(self.k.cast(), k.k, k.v) }
    }
}

/// A new entry, with its key's buffer of @key_u64s: freed if it's dropped.
struct NewEntry(NonNull<c::bkey_cached>);

impl NewEntry {
    fn alloc(key_u64s: usize) -> Option<Self> {
        let gfp = current_gfp_context_raw(c::GFP_KERNEL) | c::__GFP_ACCOUNT | c::__GFP_RECLAIMABLE;

        let mut ck = NewEntry(KEY_CACHE.zalloc(gfp)?);
        unsafe { ck.0.as_mut() }.set_key_buf(NewKey::alloc(key_u64s)?);
        Some(ck)
    }

    fn into_raw(self) -> NonNull<c::bkey_cached> {
        let ck = self.0;
        core::mem::forget(self);
        ck
    }
}

impl Drop for NewEntry {
    fn drop(&mut self) {
        unsafe { self.0.as_mut() }.free_key_buf();
        unsafe { KEY_CACHE.free(self.0) }
    }
}

/*
 * Predicate for rcu_pending_dequeue_where: try to claim a pending entry by
 * taking its intent+write locks. Succeeds only if the lock is currently idle
 * (no SRCU readers still using it), so the returned ck can be reused without
 * blocking. Called under p->lock; six_trylock is safe there (atomic ops only).
 */
fn bkey_cached_try_claim(ck: &NonNull<c::bkey_cached>) -> bool {
    bkey_cached_trylock(unsafe { ck.as_ref() })
        .map(|ck| ck.into_parts().1.into_raw())
        .is_some()
}

/// A claimed entry's hold, as bkey_cached_try_claim() left it.
///
/// # Safety
/// @ck was just claimed.
unsafe fn claimed<'t>(trans: &'t BtreeTrans<'_>, ck: NonNull<c::bkey_cached>) -> LockedEntry<'t> {
    let ck = unsafe { cached(trans, ck.as_ptr()) };
    LockedEntry::new(ck, unsafe { SixHold::from_raw(&ck.c.lock, SIX_LOCK_write) })
}

impl BtreeTrans<'_> {
    fn bkey_cached_alloc(&self, t: &TransAttempt<'_, '_>, paths: &mut Paths<'_>, btree: Btree, key_u64s: usize)
        -> Result<Option<LockedEntry<'_>>, BchError>
    {
        let bc = &self.fs().btree.key_cache;
        let pcpu_readers = btree_uses_pcpu_readers(btree);

        if let Some(ck) = bc.pending()[pcpu_readers as usize].dequeue_where(bkey_cached_try_claim) {
            return Ok(Some(unsafe { claimed(self, ck) }));
        }

        let ck = match t.allocate_dropping_locks_in(paths, || {
            NewEntry::alloc(key_u64s).ok_or(BchError::from(Errcode::ENOMEM_btree_key_cache_create))
        }) {
            Ok(ck) => Some(ck),
            Err(e) if e.matches(c::ENOMEM) => None,
            Err(e) => return Err(e),
        };

        if let Some(ck) = ck {
            let ck = ck.into_raw();
            let flags = if pcpu_readers {
                c::SixLockInitFlags::pcpu
            } else {
                c::SixLockInitFlags::empty()
            };
            unsafe {
                bch2_btree_lock_init(&raw mut (*ck.as_ptr()).c, flags, c::GFP_KERNEL);
                (*ck.as_ptr()).c.cached = true;
            }
            let ck = unsafe { cached(self, ck.as_ptr()) };
            /* Brand new lock, trylock can't fail. */
            return Ok(Some(bkey_cached_trylock(ck).expect("a new key cache entry locked by someone else")));
        }

        Ok(bc.pending()[pcpu_readers as usize].dequeue_from_all_where(bkey_cached_try_claim)
           .map(|ck| unsafe { claimed(self, ck) }))
    }
}

// ── The shrinker ─────────────────────────────────────────────────────────

impl Shrink for c::btree_key_cache_shrinker {
    fn shrinker(&self) -> &Shrinker<Self> {
        &self.shrink
    }

    fn count(&self) -> usize {
        self.container().count()
    }

    fn scan(&self, nr: usize) -> Option<usize> {
        let freed = self.container().scan(nr);
        if freed.is_some_and(|n| n > 0) {
            self.shrink_table_work.queue(Queue::system_dfl());
        }
        freed
    }
}

/// shrink_table_work: scan() runs in reclaim, and a smaller table is an
/// allocation.
pub(crate) fn shrink_table(s: &c::btree_key_cache_shrinker) {
    let _ = s.container().table.shrink();
}

impl PlacedDrop for c::btree_key_cache_shrinker {
    unsafe fn placed_drop(&self) {
        unsafe { self.shrink.unregister() }
    }
}

impl c::bch_fs_btree_key_cache {
    fn count(&self) -> usize {
        let nr = self.nr_keys.load(Relaxed) - self.nr_dirty.load(Relaxed);

        /*
         * Avoid hammering our shrinker too much if it's nearly empty - the
         * shrinker code doesn't take into account how big our cache is, if it's
         * mostly empty but the system is under memory pressure it causes nasty
         * lock contention:
         */
        (nr - 128).max(0) as usize
    }

    fn scan(&self, nr: usize) -> Option<usize> {
        let fs = self.fs();
        let (mut scanned, mut freed) = (0, 0);
        let start = self.shrink_iter.load(Relaxed);
        let mut pos = start;

        let start_time = local_clock();
        let srcu = fs.btree.trans.barrier.read_lock();
        let mut wrapped = false;

        /*
         * From where the last scan stopped, round to it again. What's evicted
         * is freed with RCU dropped - freeing can allocate, and block - so the
         * walk stops at each, and picks up from the slot after: the entries
         * are the SRCU's, which we hold, not RCU's. One at a time, not a
         * batch: this runs in reclaim, on whatever stack is left.
         */
        loop {
            let mut evicted = None;
            let (mut done, mut at_end) = (false, true);

            for (i, ck) in self.keys_from(pos, &read_lock(), &srcu) {
                if wrapped && i >= start {
                    (done, at_end) = (true, false);
                    break;
                }

                if ck.dirty() {
                    inc(&self.stats.skipped_dirty);
                } else if ck.accessed() {
                    ck.clear_flag(c::BKEY_CACHED_ACCESSED);
                    inc(&self.stats.skipped_accessed);
                } else if let Some(mut ck) = bkey_cached_lock_for_evict(ck) {
                    if self.bkey_cached_evict(&mut ck) {
                        evicted = Some(ck);
                        inc(&self.stats.freed);
                        freed += 1;
                    }
                } else {
                    inc(&self.stats.skipped_lock_fail);
                }

                pos = i + 1;
                scanned += 1;
                if scanned >= nr {
                    (done, at_end) = (true, false);
                    break;
                }
                if evicted.is_some() {
                    at_end = false;
                    break;
                }
            }

            if let Some(ck) = evicted {
                self.bkey_cached_free_noassert(ck);
            }

            if done || (at_end && wrapped) {
                break;
            }
            if at_end {
                (wrapped, pos) = (true, 0);
            }
        }

        self.shrink_iter.store(pos, Relaxed);

        fs.times[c::TimeStat::btree_key_cache_scan as usize].update(start_time);

        Some(freed)
    }

    fn fs(&self) -> &Fs {
        self.container().container()
    }

    /// Ask the shrinker to free @nr keys, as memory pressure would.
    pub fn shrink(&self, nr: c_ulong) {
        let _ = self.scan(nr as usize);
    }

    pub(crate) fn exit(&self) {
        let fs = self.fs();

        unsafe { self.shrinker.placed_drop() };

        /*
         * freed as the shrinker frees, one at a time outside RCU - not a while
         * let: its scrutinee's RCU guard would live through the free
         */
        let srcu = fs.btree.trans.barrier.read_lock();
        let mut pos = 0;
        loop {
            let evicted = self.keys_from(pos, &read_lock(), &srcu).next().map(|(i, ck)| {
                let mut ck = bkey_cached_trylock(ck).expect("a key cache entry locked at shutdown");
                assert!(self.bkey_cached_evict(&mut ck), "a key cache entry on bc->keys not in the table");
                pos = i + 1;
                ck
            });

            let Some(ck) = evicted else { break };
            self.bkey_cached_free_noassert(ck);
        }
        drop(srcu);

        let nr_dirty = self.nr_dirty.load(Relaxed);
        if nr_dirty != 0 && fs.journal.error().is_ok() && fs.flags.get(c::FsFlag::was_rw) {
            panic!("btree key cache shutdown error: nr_dirty nonzero ({nr_dirty})");
        }

        let nr_keys = self.nr_keys.load(Relaxed);
        if nr_keys != 0 {
            panic!("btree key cache shutdown error: nr_keys nonzero ({nr_keys})");
        }

        /* through pending's own pointer, which exit() may write by */
        for i in 0..2 {
            unsafe { RcuPending::exit(self.pending.get().cast::<Pending>().add(i)) };
        }
    }

    pub(crate) fn setup(&self) -> Result<(), BchError> {
        let fs = self.fs();

        let srcu = unsafe { &*ptr::from_ref(&fs.btree.trans.barrier) }.flavor();
        for i in 0..2 {
            let slot = unsafe { self.pending.get().cast::<Pending>().add(i) };
            if unsafe { init_at(RcuPending::new(srcu, Free {}), slot) }.is_err() {
                return fs.throw(Errcode::ENOMEM_fs_btree_cache_init);
            }
        }

        let mut name = Printbuf::new();
        prt!(&mut name, "{}-btree_key_cache", fs.name().to_str().unwrap_or("?"));
        Shrinker::register(&self.shrinker, name.as_c_str(), 1, 0)
            .map_err(|_| fs.err(Errcode::ENOMEM_fs_btree_cache_init))
    }
}

impl ToText for c::bch_fs_btree_key_cache {
    #[cold]
    fn to_text(&self, out: &mut Printbuf) {
        out.tabstop_push(24);
        out.tabstop_push(12);

        prt!(out, "keys:\t{}\r\n",               self.nr_keys.load(Relaxed));
        prt!(out, "dirty:\t{}\r\n",              self.nr_dirty.load(Relaxed));
        prt!(out, "table size:\t{}\r\n",         self.table.capacity());
        out.newline();
        prt!(out, "shrinker:\n");
        prt!(out, "freed:\t{}\r\n",              self.stats.freed.load(Relaxed));
        prt!(out, "skipped_dirty:\t{}\r\n",      self.stats.skipped_dirty.load(Relaxed));
        prt!(out, "skipped_accessed:\t{}\r\n",   self.stats.skipped_accessed.load(Relaxed));
        prt!(out, "skipped_lock_fail:\t{}\r\n",  self.stats.skipped_lock_fail.load(Relaxed));
        out.newline();
        prt!(out, "pending:\t{}\r\n",            self.pending().iter().map(|p| p.nr_pending()).sum::<usize>());
    }
}

pub fn btree_key_cache_module_exit() {
    #[cfg(kernel)]
    unsafe { KEY_CACHE.destroy() }
}

pub fn btree_key_cache_module_init() -> Result<(), BchError> {
    #[cfg(kernel)]
    KEY_CACHE.create(c"bkey_cached", c::SLAB_RECLAIM_ACCOUNT)
        .map_err(|_| BchError::from_raw(-(c::ENOMEM as c_int)))?;
    Ok(())
}
