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

use crate::btree::bkey::{BkeyI, BkeySC, SPOS_MAX};
use crate::data_structures::dst_room::DstRoom;
use crate::btree::iter::TransAttempt;
use crate::btree::path::{LockedNode, Paths, Write};
use crate::btree::trans::BtreeTrans;
use crate::c;
use crate::data_structures::cuckoo::{Cuckoo, Entry};
use crate::errcode::{Errcode, BchError};
use crate::format::{Bpos, Btree};
use crate::fs::Fs;
use crate::locking::six::SixHold;
use crate::prt;
use crate::util::alloc::{init_at, Box, Kmalloc};
use crate::util::hash::GoldenRatio;
use crate::os::kernel::local_clock;
use crate::os::module::Quiesced;
use crate::os::slab::KmemCache;
use crate::util::place::{Contained, PlacedDrop};
use crate::util::projections::TearSafe;
use crate::util::rcu::{read_lock, RcuHead, ReadGuard, SrcuReadGuard};
use crate::util::rcu_pending::{RcuPending, RcuRealloc};
use crate::os::shrinker::{Shrink, Shrinker};
use crate::os::workqueue::{Queue, ReffedWork};
use crate::util::{Printbuf, ToText};
use c::SixLockType::{SIX_LOCK_intent, SIX_LOCK_write};
#[cfg(kernel)]
use core::ffi::c_int;
use core::ffi::{c_uint, c_ulong};
use core::ops::Deref;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicIsize, AtomicUsize, Ordering::Relaxed};
use cstruct_macros::{placed, Init, MaybeZeroable, NoUninit, TypeInfo};
use pin_init::{pin_data, pin_init, PinInit};

pub(crate) type KeyTable = Cuckoo<Entry<c::bkey_cached>, GoldenRatio>;

#[allow(non_camel_case_types)]
#[repr(C, align(4))]
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, MaybeZeroable, NoUninit)]
pub struct bkey_cached_key {
    pub btree_id: u32,
    pub pos: Bpos,
}

// Torn, a mix of two keys: a lookup that matches one locks the entry and
// checks its key again
unsafe impl TearSafe for bkey_cached_key {}

crate::keyed_by_value!(c::bkey_cached, |ck| ck.key.get() => c::bkey_cached_key);

// what's shared is the entry lock's, published, or atomic
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

/// An entry, by its lock's readers: percpu, or not. Reused before the btree
/// trans SRCU's readers are past it, as lookups lock what they find and then
/// check its key.
#[pin_data]
#[repr(transparent)]
#[derive(RcuHead)]
pub(crate) struct Cached<const PCPU: bool> {
    #[pin]
    #[rcu_head]
    ck: c::bkey_cached,
}

unsafe impl<const PCPU: bool> RcuRealloc for Cached<PCPU> {
    /*
     * Claimed by taking its intent+write locks: only if the lock is idle (no
     * SRCU readers still using it), so it can be reused without blocking.
     * Called under p->lock; six_trylock is safe there (atomic ops only).
     */
    fn try_claim(&self) -> bool {
        bkey_cached_trylock(&self.ck)
            .map(|ck| ck.into_parts().1.into_raw())
            .is_some()
    }

    fn init() -> impl PinInit<Self, BchError> {
        let flags = if PCPU {
            c::SixLockInitFlags::pcpu
        } else {
            c::SixLockInitFlags::empty()
        };
        pin_init!(Self { ck <- c::bkey_cached::init(flags) }? BchError)
    }
}

pub(crate) type Pending<const PCPU: bool> = RcuPending<'static, Cached<PCPU>, &'static KmemCache<c::bkey_cached>>;
pub(crate) type Pendings = (Pending<false>, Pending<true>);

#[allow(non_camel_case_types)]
#[placed(c::bch_fs_btree.key_cache)]
#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, TypeInfo, Init)]
pub struct bch_fs_btree_key_cache {
    #[pin]
    #[init(::pin_init::init_zeroed())]
    pub(crate) table: KeyTable,
    /* what's in the table, to walk: an entry's slot is from insert to evict */
    #[pin]
    #[init(crate::util::alloc::init_or(c::fast_list::new(), c::Errcode::ENOMEM_fs_btree_key_cache_init))]
    pub(crate) keys: c::fast_list,

    #[pin]
    pub(crate) shrinker: c::btree_key_cache_shrinker,
    #[pin]
    #[init(AtomicUsize::new(0))]
    pub(crate) shrink_iter: AtomicUsize,

    /* 0: non pcpu reader locks, 1: pcpu reader locks */
    #[pin]
    #[init(::pin_init::init_zeroed())]
    /* set up and exited through the struct shared, in place: setup(), exit() */
    pub(crate) pending: core::cell::UnsafeCell<Pendings>,

    pub nr_keys: AtomicIsize,
    pub nr_dirty: AtomicIsize,

    /* shrinker stats */
    #[pin]
    #[init(ShrinkStats::new())]
    pub(crate) stats: ShrinkStats,
}

impl bch_fs_btree_key_cache {
    pub(crate) fn pending(&self) -> &Pendings {
        unsafe { &*self.pending.get() }
    }

    pub fn nr_keys_need_flush(&self) -> usize {
        let nr_dirty = self.nr_dirty.load(Relaxed);
        let nr_keys = self.nr_keys.load(Relaxed);
        let max_dirty = 1024 + nr_keys / 2;

        nr_dirty.saturating_sub(max_dirty).max(0) as usize
    }

    pub fn dirty_over_max(&self) -> isize {
        let nr_dirty = self.nr_dirty.load(Relaxed);
        let nr_keys = self.nr_keys.load(Relaxed);
        let max_dirty = 4096 + (nr_keys * 3) / 4;

        nr_dirty - max_dirty
    }

    pub fn must_wait(&self) -> bool {
        self.dirty_over_max() > 0
    }

    pub fn wait_done(&self) -> bool {
        let nr_dirty = self.nr_dirty.load(Relaxed);
        let nr_keys = self.nr_keys.load(Relaxed);
        let max_dirty = 2048 + (nr_keys * 5) / 8;

        nr_dirty <= max_dirty
    }
}

/// An entry, held write locked: what its contents are written through.
pub(super) struct LockedEntry<'a> {
    ck:   LockedNode<'a, c::bkey_cached, Write>,
    held: SixHold<'a>,
}

impl<'a> LockedEntry<'a> {
    fn new(ck: &'a c::bkey_cached, held: SixHold<'a>) -> Self {
        assert!(held.ty() == SIX_LOCK_write && ptr::eq(held.lock(), &ck.c.lock),
                "a key cache entry's handle without its write lock");
        LockedEntry { ck: unsafe { LockedNode::new_write(ck) }, held }
    }

    fn into_parts(self) -> (&'a c::bkey_cached, SixHold<'a>) {
        (self.ck.node(), self.held)
    }

    fn contents(&self) -> &EntryContents {
        self.ck.contents()
    }

    fn contents_mut(&mut self) -> &mut EntryContents {
        self.ck.contents_mut()
    }
}

impl Deref for LockedEntry<'_> {
    type Target = c::bkey_cached;

    fn deref(&self) -> &c::bkey_cached {
        self.ck.node()
    }
}

fn bkey_cached_trylock(ck: &c::bkey_cached) -> Option<LockedEntry<'_>> {
    let intent = ck.c.lock.try_lock_as(SIX_LOCK_intent)?;
    Some(LockedEntry::new(ck, intent.try_write().ok()?))
}

impl BtreeTrans<'_> {
    /// @pos's entry, found unlocked: for as long as the transaction holds the
    /// btree trans SRCU, which an entry is freed a grace period of.
    pub(crate) fn btree_key_cache_find(&self, btree: Btree, pos: Bpos) -> Option<&c::bkey_cached> {
        let key = c::bkey_cached_key { btree_id: btree.0, pos };

        self.fs().btree.key_cache.table.get_entry(&key, &read_lock()).map(|ck| unsafe { &*ptr::from_ref(ck) })
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
        let ret = self.table.remove_entry(&ck.key.get(), NonNull::from(ck.ck.node()));
        if ret {
            self.keys.remove(ck.contents().keys_idx);
            ck.contents_mut().keys_idx = 0;
            ck.key.set(c::bkey_cached_key { btree_id: !0, pos: SPOS_MAX });
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
        ck.contents_mut().keys_idx = self.keys.add(NonNull::from(ck.ck.node()).cast())
            .map_err(|_| BchError::from(Errcode::ENOMEM_btree_key_cache_create))?;
        Ok(())
    }

    fn bkey_cached_free_noassert(&self, mut ck: LockedEntry<'_>) {
        ck.contents_mut().k = None;

        let (ck, held) = ck.into_parts();
        drop(held);

        /* by the lock it has: percpu read counts may not have been allocated */
        let pcpu_readers = ck.c.lock.six().readers.allocated();
        let ck = NonNull::from(ck);
        unsafe {
            if pcpu_readers {
                self.pending().1.free(ck.cast())
            } else {
                self.pending().0.free(ck.cast())
            }
        }
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

/// An entry's key buffer: kmalloc()'d, as C's commit path reallocs and
/// kfree()s it.
pub(crate) type KeyBuf = Box<DstRoom<BkeyI>, Kmalloc>;

/// A key buffer of @u64s, holding a deleted key.
pub(crate) fn key_buf(u64s: usize) -> Option<KeyBuf> {
    DstRoom::new_boxed_in(u64s, Kmalloc).ok()
}

/// What an entry's lock protects: written with it write locked, read with it
/// held.
#[derive(Default, MaybeZeroable)]
pub struct EntryContents {
    pub(crate) keys_idx: c_uint,
    /* its newest update's journal seq, which its pin is moved up to */
    pub(crate) seq: u64,
    /* its key, in a buffer of its own: none once it's freed */
    pub(crate) k: Option<KeyBuf>,
}

impl EntryContents {
    /// Its key buffer's room, in u64s: 0 for no buffer.
    pub(crate) fn key_room(&self) -> usize {
        self.k.as_ref().map_or(0, |k| k.elems().len())
    }

    pub(crate) fn key(&self) -> &BkeyI {
        self.k.as_deref().expect("a key cache entry's key")
    }

    fn set_key(&mut self, k: BkeySC<'_>) {
        self.k.as_mut().expect("a key cache entry's key buffer").set_key(k)
    }
}

/// A new entry, from @pending's alloc(), claimed, with its key's buffer: back
/// to it, its key buffer freed and its locks dropped, if it's dropped - as
/// when its key buffer can't be had, or relocking after it fails.
struct NewEntry<'a, const PCPU: bool> {
    pending: &'a Pending<PCPU>,
    /* taken by into_entry() */
    ck:      Option<LockedEntry<'a>>,
}

impl<'a, const PCPU: bool> NewEntry<'a, PCPU> {
    fn alloc(t: &TransAttempt<'_, '_>, paths: &mut Paths<'_>, pending: &'a Pending<PCPU>, key_u64s: usize)
        -> Result<LockedEntry<'a>, BchError>
    {
        let enomem = || BchError::from(Errcode::ENOMEM_btree_key_cache_create);

        t.allocate_dropping_locks_in(paths, || {
            let ck = pending.alloc().map_err(|_| enomem())?;
            let mut ck = NewEntry { pending, ck: Some(unsafe { claimed(ck.cast()) }) };
            let k = key_buf(key_u64s).ok_or_else(enomem)?;
            ck.ck.as_mut().expect("a new entry").contents_mut().k = Some(k);
            Ok(ck)
        }).map(NewEntry::into_entry)
    }

    fn into_entry(mut self) -> LockedEntry<'a> {
        self.ck.take().expect("a new entry")
    }
}

impl<const PCPU: bool> Drop for NewEntry<'_, PCPU> {
    fn drop(&mut self) {
        let Some(mut ck) = self.ck.take() else { return };
        ck.contents_mut().k = None;
        let (ck, held) = ck.into_parts();
        drop(held);
        unsafe { self.pending.free(NonNull::from(ck).cast()) }
    }
}

/// A claimed entry's hold, as Cached's try_claim() left it.
///
/// # Safety
/// @ck was just claimed, and is had for 'a.
unsafe fn claimed<'a>(ck: NonNull<c::bkey_cached>) -> LockedEntry<'a> {
    let ck: &'a c::bkey_cached = unsafe { ck.as_ref() };
    LockedEntry::new(ck, unsafe { SixHold::from_raw(&ck.c.lock, SIX_LOCK_write) })
}

impl BtreeTrans<'_> {
    /// An entry with its key's buffer of @key_u64s, locked; None if there's no
    /// memory for one.
    fn bkey_cached_alloc(&self, t: &TransAttempt<'_, '_>, paths: &mut Paths<'_>, btree: Btree, key_u64s: usize)
        -> Result<Option<LockedEntry<'_>>, BchError>
    {
        let bc = &self.fs().btree.key_cache;

        let ck = if btree_uses_pcpu_readers(btree) {
            NewEntry::alloc(t, paths, &bc.pending().1, key_u64s)
        } else {
            NewEntry::alloc(t, paths, &bc.pending().0, key_u64s)
        };
        match ck {
            Ok(ck) => Ok(Some(ck)),
            Err(e) if e.matches(c::ENOMEM) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

// ── The shrinker ─────────────────────────────────────────────────────────

/// The key cache's shrinker, Sync - a shrinker's owner is - as the key cache,
/// with its per-CPU counter and fast_list, isn't.
#[allow(non_camel_case_types)]
#[placed(c::bch_fs_btree_key_cache.shrinker)]
#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, Init)]
pub struct btree_key_cache_shrinker {
    pub(crate) shrink: Shrinker<Self>,
    /* the table shrunk once scan() has emptied it: its allocation, off reclaim */
    #[pin]
    pub(crate) shrink_table_work: ReffedWork<shrink_table, crate::field!(super.super.super.ro_ref)>,
}

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

        fs.times.get(c::TimeStat::btree_key_cache_scan as usize).update(start_time);

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
        let pending = self.pending.get();
        unsafe {
            RcuPending::exit(&raw mut (*pending).0);
            RcuPending::exit(&raw mut (*pending).1);
        }
    }

    pub(crate) fn setup(&self) -> Result<(), BchError> {
        let fs = self.fs();

        let srcu = unsafe { &*ptr::from_ref(&fs.btree.trans.barrier) }.flavor();
        let pending = self.pending.get();
        if unsafe { init_at(RcuPending::new(srcu, &KEY_CACHE), &raw mut (*pending).0) }.is_err() ||
           unsafe { init_at(RcuPending::new(srcu, &KEY_CACHE), &raw mut (*pending).1) }.is_err() {
            return fs.throw(Errcode::ENOMEM_fs_btree_cache_init);
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
        prt!(out, "pending:\t{}\r\n",            self.pending().0.nr_pending() + self.pending().1.nr_pending());
    }
}

pub fn btree_key_cache_module_exit(_q: &Quiesced<'_>) {
    #[cfg(kernel)]
    KEY_CACHE.destroy(_q)
}

pub fn btree_key_cache_module_init() -> Result<(), BchError> {
    #[cfg(kernel)]
    KEY_CACHE.create(c"bkey_cached", c::SLAB_RECLAIM_ACCOUNT | c::SLAB_ACCOUNT)
        .map_err(|_| BchError::from_raw(-(c::ENOMEM as c_int)))?;
    Ok(())
}
