// SPDX-License-Identifier: GPL-2.0

/* DOC(btree-node-cache)
 *
 * In-memory btree nodes are kept in a hash table indexed by their physical
 * on-disk pointer (not their logical position). This is because btree node
 * split and compact operations are copy-on-write: new nodes are allocated, the
 * parent is updated to point to them, and the old nodes are freed. Indexing by
 * physical pointer avoids the need to atomically update the hash table during
 * these operations.
 *
 * The `struct btree` objects themselves are never freed during normal operation
 * (only at shutdown), which means locks can be dropped and retaken without
 * reference counting. This enables the aggressive lock-dropping discipline
 * that keeps btree lock hold times bounded to in-memory operations: after
 * dropping a lock, a sequence number check determines whether the node has
 * changed and needs to be re-traversed.
 *
 * Btree roots are pinned in memory and accessed directly. All other nodes may
 * be evicted from the cache and reused for different on-disk nodes at any time
 * when unlocked, so after locking a node the caller must verify it is still
 * the expected node.
 *
 * Membership state machine
 * ------------------------
 *
 * A struct btree is in exactly one of five states at any time, recorded in
 * b->cache_state and maintained by bch2_btree_node_transition_state():
 *
 *   NONE      — off all lists, NOT hashed. Either kzalloc'd and not yet
 *               placed, transiently held by one thread mid-state-change,
 *               or claimed out of the cache by a caller for private use.
 *   FREED     — bc->freed_pcpu (level-0 pcpu-readers locks) or
 *               bc->freed_nonpcpu (interior nodes), NOT hashed, b->data NULL.
 *               struct btree shell pool: lock state preserved, data buffer
 *               released. Not counted (freed lists are unbounded scratch).
 *   FREEABLE  — bc->freeable, NOT hashed, b->data set. Buffer cache for hot
 *               alloc/free churn. Counted in bc->nr_freeable.
 *   CLEAN     — bc->live[btree_node_pinned(b)].clean, hashed, b->data set,
 *               BTREE_NODE_dirty clear. Findable by lookup; the shrinker
 *               walks this list to reclaim. Counted in
 *               bc->live_nr[pinned].nr_clean and bc->nr_by_btree[].
 *   DIRTY     — bc->live[btree_node_pinned(b)].dirty, hashed, b->data set,
 *               BTREE_NODE_dirty set. Counted in bc->live_nr[pinned].nr_dirty
 *               and bc->nr_by_btree[]. Not shrinker-eligible until written
 *               out and transitioned to CLEAN.
 *
 * Roots are CLEAN or DIRTY with BTREE_NODE_permanent set; the flag tells
 * the reclaim path to skip them. Roots are also tracked via
 * bc->roots_known[]/roots_extra (under bc->root_lock).
 *
 * All transitions go through one primitive:
 *
 *   bch2_btree_node_transition_state(c, b, target)
 *      Takes bc->lock, detaches from b's current state (undoing hash/list/
 *      counter bookkeeping) then attaches to @target. Idempotent for
 *      current==target. Transition to CLEAN/DIRTY can fail (hash table
 *      insert collision) on the un-hashed→hashed transition; on failure
 *      b->hash_val is reset and b ends up in FREEABLE. Internal callers
 *      already holding bc->lock use the static _locked variant.
 *
 * Freeing a node's buffers splits the transition: OwnedNode::take() goes to
 * NONE, undoing the old state's bookkeeping while data is still attached, then
 * OwnedNode::free() frees data and lists the node FREED.
 *
 * One non-state-machine transition: any live state → same state +
 * BTREE_NODE_permanent (bch2_btree_set_root_inmem just sets the flag; the
 * node stays put). Eviction (node_evict()) BUG_ONs on permanent
 * nodes — there's no legitimate caller path.
 *
 * Lock discipline:
 *
 *   bc->lock         — list membership, hash table mutation, counters
 *   bc->root_lock    — roots_known[]/roots_extra slot
 *   b->c.lock        — per-node six_lock (read/intent/write)
 *   bc->alloc_lock   — cmpxchg'd task pointer; serialises cannibalize so
 *                      only one thread reclaims under memory pressure
 *
 * Hash table lookups are RCU-safe; lookups can race any LIVE↔NONE
 * transition and may briefly miss a node that's hash-removed mid-flight.
 * After locking a node returned by lookup, callers must recheck b->hash_val
 * against the expected key to detect reuse.
 */

#![allow(non_camel_case_types)]

mod alloc;
mod bufs;
mod get;
mod init;
mod pins;
mod reclaim;
mod state;

use bufs::cache_node_struct_alloc;
pub(crate) use bufs::node_mem_alloc;
pub(crate) use bufs::cached_node;
pub(crate) use alloc::OwnedNode;
pub use bufs::NodeBufs;
pub use bufs::{bounce_bufs, node_mem_free, Bounce, BounceBufs};
pub use pins::BtreeCachePins;
pub(crate) use state::NodeTable;

use crate::btree::bkey::{bkey_val_to_text, val_to_text, BkeyI, BkeySC, BkeyVal, BKEY_U64S};
use crate::data_structures::dst_room::DstRoom;
use crate::btree::iter::BtreeTrans;
use crate::btree::node::bset::BsetStats;
use crate::btree::node::{BtreeNode, NodeContents};
use crate::util::alloc::{Box, Global};
use crate::util::place::{Contained, PlacedDrop};
use crate::os::shrinker::{Shrink, Shrinker};
use crate::util::sleep::Sleep;
use crate::c;
use crate::debug::trace::events;
use crate::errcode::{Errcode, BchError};
use crate::prt;
use crate::os::sched::current;
use crate::fs::Fs;
use crate::util::vstructs::Head;
use core::mem::offset_of;
use crate::util::{HumanReadable, Printbuf, ToText};
use core::ffi::{c_int, c_uint};
use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering::{Relaxed, Release, SeqCst};
use crate::format::{Btree, BkeyType};
use crate::btree::interior::{BtreeRoots, BtreeRootsLive};
use core::sync::atomic::{AtomicBool, AtomicIsize};
use crate::locking::{ExclusiveGuard, Locked, Memalloc, Mutex, PF_MEMALLOC_NOIO, PF_MEMALLOC_NORECLAIM, PF_MEMALLOC_NOWARN};
use crate::types::CacheAligned;
use crate::util::projections::Volatile;
use cstruct_macros::{bch_enum, Init, MaybeZeroable, placed, TypeInfo};
use pin_init::pin_data;
#[cfg(kernel)]
use kernel::sync::SetOnce;
#[cfg(not(kernel))]
use bcachefs_shim::sync::SetOnce;

/*
 * Membership state of a struct btree in the btree node cache.
 *
 * Stored in b->cache_state and maintained by btree_node_transition_state().
 * See DOC(btree-node-cache), in btree/cache/mod.rs, for the state machine and
 * the bookkeeping each state implies.
 */
#[bch_enum(closed)]
#[repr(u32)]
pub enum BtreeNodeCacheState {
    NONE,           /* off all lists; not in cache (kzalloc default) */
    FREED,          /* on bc->freed_{pcpu,nonpcpu}; no data buffer */
    FREEABLE,       /* on bc->freeable; has data; not hashed */
    CLEAN,          /* on bc->live[pinned].clean; hashed; has data */
    DIRTY,          /* on bc->live[pinned].dirty; hashed; has data */
}

#[bch_enum(closed)]
#[nr]
#[repr(u32)]
pub enum BtreeCacheNotFreedReason {
    cache_reserve,
    lock_intent,
    lock_write,
    dirty,
    read_in_flight,
    write_in_flight,
    permanent,
    noevict,
    write_blocked,
    will_make_reachable,
    access_bit,
}

/// The node cache's lists: intrusive, one of list.rs's exceptions - a node
/// moves between them on every state change, in place, and is never freed
/// while the fs runs.
pub type BtreeNodeList = crate::list!(BtreeNode, list);

#[repr(C)]
#[derive(MaybeZeroable)]
pub struct btree_cache_list {
    pub clean: BtreeNodeList,
    pub dirty: BtreeNodeList,
}

/// live[idx]'s shrinker: not bc->lock's, as the lists are - the shrinker
/// runs without it.
#[placed(c::bch_fs_btree_cache.shrinkers[], index = idx: u32)]
#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, Init)]
pub struct btree_cache_shrinker {
    pub(crate) shrink: Shrinker<Self>,
}

/// bc->lock's: the live lists - unpinned and pinned - the freeable and freed
/// lists, the counts, and the window of nodes pinned.
#[repr(C)]
#[derive(MaybeZeroable)]
pub struct BtreeCacheLists {
    pub live: [c::btree_cache_list; 2],
    pub freeable: BtreeNodeList,
    pub freed_pcpu: BtreeNodeList,
    pub freed_nonpcpu: BtreeNodeList,

    pub nr_vmalloc: usize,
    pub nr_by_btree: [usize; Btree::NR as usize],

    pub pinned_nodes_start: c::bbpos,
    pub pinned_nodes_end: c::bbpos,
    /* btree id mask: 0 for leaves, 1 for interior */
    pub pinned_nodes_mask: [u64; 2],

    /* shrinker stats */
    pub nr_freed: usize,
    pub nr_requested: usize,
    pub not_freed: [u64; c::BtreeCacheNotFreedReason::NR as usize],
}

/// bc->lock: its holder never enters reclaim, which would take it again
/// through the node cache's shrinker. NOIO too, for the kernel's own
/// allocations, which don't know NORECLAIM.
pub(crate) type BtreeCacheMutex = Memalloc<Mutex, { PF_MEMALLOC_NOIO | PF_MEMALLOC_NORECLAIM | PF_MEMALLOC_NOWARN }>;

/// bc->lock, held.
pub type BtreeCacheGuard<'a> =
    ExclusiveGuard<'a, BtreeCacheMutex, BtreeCacheLists, BtreeCacheLive, crate::locking::Exclusive, BtreeCacheLockAt>;

/// bc->lock's place, for what it covers to name.
pub type BtreeCacheLockAt = crate::at!(bch_fs_btree_cache.lock);

crate::published! {
    /// A live list's nodes, clean and dirty: changed with its list, read
    /// without bc->lock - the shrinker's count, the write throttle.
    #[derive(MaybeZeroable)]
    pub struct BtreeCacheLiveNr {
        pub nr_clean: Volatile<usize>,
        pub nr_dirty: Volatile<usize>,
    }
}

crate::published! {
    #[derive(MaybeZeroable)]
    pub struct BtreeCacheLive {
        pub live_nr: [BtreeCacheLiveNr; 2],
        /* read by the shrinker's count and the allocator, without bc->lock */
        pub nr_freeable: Volatile<usize>,
        pub nr_reserve: Volatile<usize>,
    }
}

impl BtreeCacheLive {
    pub fn nr_live(&self) -> usize {
        self.live_nr.iter().map(|l| l.nr_clean.get() + l.nr_dirty.get()).sum()
    }

    pub fn nr_dirty(&self) -> usize {
        self.live_nr.iter().map(|l| l.nr_dirty.get()).sum()
    }
}

#[placed(c::bch_fs_btree.cache)]
#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, TypeInfo, Init)]
pub struct bch_fs_btree_cache {
    #[pin]
    #[init(crate::new_lock!(<- BtreeRoots::init(), ::pin_init::zeroed(), "&bc->root_lock"))]
    pub roots: Locked<Mutex, BtreeRoots, BtreeRootsLive>,

    #[pin]
    #[init(::pin_init::init_zeroed())]
    pub(crate) table: crate::btree::cache::NodeTable,
    /*
     * We never free a struct btree, except on shutdown - we just put it on
     * the btree_cache_freed list and reuse it later. This simplifies the
     * code, and it doesn't cost us much memory as the memory usage is
     * dominated by buffers that hold the actual btree node data and those
     * can be freed - and the number of struct btrees allocated is
     * effectively bounded.
     *
     * btree_cache_freeable effectively is a small cache - we use it because
     * high order page allocations can be rather expensive, and it's quite
     * common to delete and allocate btree nodes in quick succession. It
     * should never grow past ~2-3 nodes in practice.
     */
    #[pin]
    #[init(crate::new_lock!(<- ::pin_init::init_zeroed(), ::pin_init::zeroed(), "&bc->lock"))]
    pub lock: crate::util::place::Placed<Locked<BtreeCacheMutex, BtreeCacheLists, BtreeCacheLive>, BtreeCacheLockAt>,
    #[pin]
    pub shrinkers: [c::btree_cache_shrinker; 2],

    /*
     * Every struct btree the cache has, whatever its state: to walk them all
     * without bc->lock. A node keeps its slot until it's freed, at exit, so a
     * walk can drop RCU and pick up from where it was. Node scan's buffers
     * aren't the cache's, and are freed while we run: they're not on it.
     */
    #[pin]
    #[init(crate::util::alloc::init_or(c::fast_list::new(), c::Errcode::ENOMEM_fs_btree_cache_init))]
    pub nodes: c::fast_list,

    /* Number of nodes with BTREE_NODE_write_in_flight set. */
    #[pin]
    #[init(AtomicIsize::new(0))]
    pub nr_in_flight: AtomicIsize,
    pub nr_in_flight_inner: AtomicIsize,
    pub nr_in_flight_wait: crate::util::futex::EventCount,
    pub __should_throttle_align: CacheAligned,
    pub should_throttle: AtomicBool,

    /*
     * Times the allocator hit the memory-pressure self reclaim path:
     * journal replay watches for this going nonzero to switch off the
     * sorted-order fastpath, which holds every journal pin until replay
     * finishes - pins must be released for reclaim to clean btree nodes.
     */
    #[pin]
    #[init(AtomicIsize::new(0))]
    pub nr_self_reclaim: AtomicIsize,

    /*
     * If we need to allocate memory for a new btree node and that
     * allocation fails, we can cannibalize another node in the btree cache
     * to satisfy the allocation - lock to guarantee only one thread does
     * this at a time:
     */
    #[pin]
    #[init(core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()))]
    pub alloc_lock: core::sync::atomic::AtomicPtr<c::task_struct>,
    #[pin]
    #[init(crate::util::futex::EventCount::new())]
    pub alloc_wait: crate::util::futex::EventCount,
}

/*
 * Sidecar cache: when a btree node is evicted (transitioned out of the in-cache
 * hash table) we stash its live_u64s here, keyed by btree_ptr_hash_val. The
 * merge gate uses this as a cheap "what would be the combined size?" estimate
 * for siblings that aren't currently in cache, before paying for a real read.
 *
 * Fixed-size, no chaining: insert overwrites whatever's in the slot. Lookup
 * verifies the stored hash matches the request; collisions naturally degrade
 * to "no info" and the caller falls back to reading the sibling. Sized at
 * ~capacity/1000/btree_node_size entries, which keeps memory tiny.
 */
#[repr(C)]
pub struct btree_evicted_size {
    /* a power of 2 of them; none until btree_init_rw() */
    pub entries: SetOnce<Box<[AtomicU64]>>,
}

pub const BTREE_EVICTED_SIZE_HASH_BITS: u32 = 48;
pub const BTREE_EVICTED_SIZE_HASH_MASK: u64 = (1 << c::BTREE_EVICTED_SIZE_HASH_BITS) - 1;


impl c::btree_evicted_size {
    fn pack(hash: u64, live_u64s: u16) -> u64 {
        ((hash & c::BTREE_EVICTED_SIZE_HASH_MASK) << 16) | u64::from(live_u64s)
    }

    fn slot(&self, hash: u64) -> Option<&AtomicU64> {
        let t = self.entries.as_ref()?;
        Some(&t[hash as usize & (t.len() - 1)])
    }

    /// An evicted node's live u64s, by its pointer's hash.
    pub fn record(&self, hash: u64, live_u64s: u16) {
        if let Some(e) = self.slot(hash) {
            e.store(Self::pack(hash, live_u64s), Relaxed);
        }
    }

    /// The live u64s recorded for @hash, if its slot still has them.
    pub fn lookup(&self, hash: u64) -> Option<u16> {
        let entry = self.slot(hash)?.load(Relaxed);
        ((entry >> 16) == (hash & c::BTREE_EVICTED_SIZE_HASH_MASK)).then_some(entry as u16)
    }

    /// The table for @fs, sized by its capacity: for btree_init_rw(). Made
    /// once - read meanwhile, by evictions, as none.
    pub fn alloc(&self, fs: &Fs) -> Result<(), BchError> {
        /*
         * 128K-entry floor (1 MiB), then add one bit (double the table) for
         * each four doublings of the maximum possible btree node count above
         * 4M nodes (~1 TiB at 256K nodes). Keeps the table modest on huge
         * filesystems: 1 TiB → 1 MiB, 1 PiB → 4 MiB, 256 PiB → 16 MiB.
         */
        let max_nodes = fs.capacity.capacity.get() / u64::from(fs.opts.btree_node_size.get() >> 9).max(1);
        let mut bits = 17;
        if max_nodes > 1 << 22 {
            bits += (max_nodes.ilog2() - 22) / 4;
        }
        let nr = 1usize << bits;

        let Ok(entries) = Box::try_new_zeroed_slice_in(nr, Global) else {
            return fs.throw(Errcode::ENOMEM_fs_other_alloc);
        };
        // an AtomicU64 may be zero
        self.entries.populate(unsafe { entries.assume_init() });
        Ok(())
    }
}

/// A node pointer's identity in the node cache - a v2's seq, a v1's first
/// pointer - read as C reads it, the bytes as they are: btree_ptr_hash_val().
/// 0 for a v1 with no pointer - only a fake root's key, a node's own: see
/// node_key_hash_val().
pub fn btree_ptr_hash_val<V: ?Sized + BkeyVal>(k: BkeySC<'_, V>) -> u64 {
    let offset = match k.k.key_type() {
        BkeyType::btree_ptr    => 0,
        BkeyType::btree_ptr_v2 => offset_of!(c::BtreePtrV2, repr.seq),
        _ => return 0,
    };
    k.val_bytes().get(offset..offset + 8)
        .map_or(0, |b| u64::from_ne_bytes(b.try_into().expect("8 bytes")))
}

/// btree_ptr_hash_val() of a node's own key, which has room past its end: a
/// v1's first value u64, there or not, as C reads it - a fake root's key has
/// no value, and its identity is the u64 past it.
pub fn node_key_hash_val(k: &DstRoom<BkeyI>) -> u64 {
    match k.k.key_type() {
        BkeyType::btree_ptr => k.elems()[BKEY_U64S],
        _ => btree_ptr_hash_val(BkeySC::from(&**k)),
    }
}

pub const BTREE_WRITE_IO_LIMIT: u32 = 64;

impl c::bch_fs_btree_cache {
    pub fn update_throttle(&self) {
        let (live, dirty) = (self.lock.nr_live(), self.lock.nr_dirty());
        let throttle = self.nr_in_flight_inner.load(Relaxed) > BTREE_WRITE_IO_LIMIT as isize ||
            (live != 0 && dirty > live * 3 / 4);

        if throttle != self.should_throttle.load(Relaxed) {
            self.should_throttle.store(throttle, Relaxed);
        }
    }

    /// Ask the shrinker to free @nr nodes, as memory pressure would. Not
    /// under bc->lock: the scan takes it.
    pub fn shrink(&self, nr: core::ffi::c_ulong) {
        let _ = self.shrinkers[0].scan(nr as usize);
    }
}

impl BtreeNode {
    /// The hashed (live) cache state derived from b's flag bits. A node
    /// belongs on live[].dirty whenever there's pending work - either
    /// BTREE_NODE_dirty (needs a write) or BTREE_NODE_write_in_flight (one is
    /// outstanding); otherwise it belongs on live[].clean.
    ///
    /// Use this anywhere you need to compute the right cache_state from the
    /// current bits - passing it to transition_state_locked() is a no-op when
    /// the node is already in the right state, so callers don't need "do we
    /// need to transition?" conditionals.
    pub fn live_state(&self) -> c::BtreeNodeCacheState {
        let f = self.flags.load();
        if f.dirty() || f.write_in_flight() {
            c::BtreeNodeCacheState::DIRTY
        } else {
            c::BtreeNodeCacheState::CLEAN
        }
    }
}

impl BtreeNode {
    pub fn wait_on_flag(&self, flag: fn(&c::BtreeNodeFlags) -> bool) {
        let Ok(()) = self.flags.wait_until(Sleep::UNINTERRUPTIBLE.io(), |f| !flag(&f));
    }
}

impl Fs {
    /// Bytes of btree nodes cached.
    pub fn btree_cache_size(&self) -> u64 {
        let bc = &self.btree.cache;
        (bc.lock.nr_live() * self.opts.btree_node_size.get() as usize) as u64
    }
}

// ── The cannibalize lock ─────────────────────────────────────────────────

/*
 * We can only have one thread cannibalizing other cached btree nodes at a time,
 * or we'll deadlock. We use an open coded mutex to ensure that, which a
 * cannibalize_bucket() will take. This means every time we unlock the root of
 * the btree, we need to release this lock if we have it held.
 */
impl BtreeTrans<'_> {
    /// The cannibalize lock, this thread's - or, held by another,
    /// btree_cache_cannibalize_lock_blocked if @blocking: the caller drops its
    /// btree locks, then waits on alloc_wait from a snapshot taken before
    /// this. Not @blocking, ENOMEM_btree_cache_cannibalize_lock.
    pub fn btree_cache_cannibalize_trylock(&self, blocking: bool) -> Result<(), BchError> {
        let fs = self.fs();
        let me = current();

        match fs.btree.cache.alloc_lock.compare_exchange(core::ptr::null_mut(), me, SeqCst, Relaxed) {
            Err(old) if old != me => {
                fs.event_inc_trace(events::btree_cache_cannibalize_lock_fail, |out| prt!(out, "{}", self.fn_name()));
                fs.throw(if blocking {
                    Errcode::btree_cache_cannibalize_lock_blocked
                } else {
                    Errcode::ENOMEM_btree_cache_cannibalize_lock
                })
            }
            _ => {
                self.set_btree_cache_cannibalize_locked(true);
                fs.event_inc_trace(events::btree_cache_cannibalize_lock, |out| prt!(out, "{}", self.fn_name()));
                Ok(())
            }
        }
    }

    /// The cannibalize lock, waited for: with no btree locks held.
    pub fn btree_cache_cannibalize_lock(&self) {
        let wait = &self.fs().btree.cache.alloc_wait;
        loop {
            let seen = wait.snapshot();
            if self.btree_cache_cannibalize_trylock(true).is_ok() {
                return;
            }
            let Ok(()) = wait.wait(seen, Sleep::UNINTERRUPTIBLE);
        }
    }

    pub fn btree_cache_cannibalize_unlock(&self) {
        let fs = self.fs();
        let bc = &fs.btree.cache;

        if bc.alloc_lock.load(Relaxed) == current() {
            fs.event_inc_trace(events::btree_cache_cannibalize_unlock, |out| prt!(out, "{}", self.fn_name()));
            // before the wake: a waiter whose snapshot sees it sees the lock free
            bc.alloc_lock.store(core::ptr::null_mut(), Release);
            bc.alloc_wait.wake();
        }
        self.set_btree_cache_cannibalize_locked(false);
    }
}

// ── The shrinkers ────────────────────────────────────────────────────────

/// What a node costs to recreate, by live list: a pinned one, 4x.
const SEEKS_MULT: [c_int; 2] = [1, 4];

impl Shrink for c::btree_cache_shrinker {
    fn shrinker(&self) -> &Shrinker<Self> {
        &self.shrink
    }

    fn count(&self) -> usize {
        self.container().container().container().btree_cache_count(self.idx.get() as usize)
    }

    fn scan(&self, nr: usize) -> Option<usize> {
        self.container().container().container().btree_cache_scan(self.idx.get() as usize, nr)
    }
}

impl PlacedDrop for c::btree_cache_shrinker {
    unsafe fn placed_drop(&self) {
        unsafe { self.shrink.unregister() }
    }
}

impl c::bch_fs_btree_cache {
    pub fn shrinkers_register(&self, fs: &Fs) -> Result<(), BchError> {
        let seeks = c_int::from(fs.opts.btree_cache_shrinker_seeks.get());
        let lists = ["btree_cache", "btree_cache-pinned"].into_iter().zip(SEEKS_MULT);
        for (s, (list, mult)) in self.shrinkers.iter().zip(lists) {
            let mut name = Printbuf::new();
            prt!(&mut name, "{}-{list}", fs.name().to_str().unwrap_or("?"));
            Shrinker::register(s, name.as_c_str(), seeks * mult, 0)
                .map_err(|_| fs.err(Errcode::ENOMEM_fs_btree_cache_init))?;
        }
        Ok(())
    }

    pub fn set_shrinker_seeks(&self, seeks: c_int) {
        for (s, mult) in self.shrinkers.iter().zip(SEEKS_MULT) {
            s.shrink.set_seeks(seeks * mult);
        }
    }
}

impl Fs {
    pub fn recalc_btree_reserve(&self) {
        let mut reserve = 16;

        if self.btree_id_root(0).is_none() {
            reserve += 8;
        }

        for i in 0..self.btree_id_nr_alive() {
            if let Some(b) = self.btree_id_root(i) {
                reserve += usize::from(b.c.level.get()).min(1) * 8;
            }
        }

        self.btree.cache.lock.lock().published().nr_reserve.set(reserve);
    }
}

// ── Text ─────────────────────────────────────────────────────────────────

pub fn bch2_btree_id_level_to_text(out: &mut Printbuf, btree: Btree, level: c_uint) {
    prt!(out, "btree={btree} level={level}");
}

pub fn btree_pos_to_text(out: &mut Printbuf, fs: &Fs, btree: Btree, level: c_uint, k: BkeySC<'_>) {
    prt!(out, "{btree} level {level}/");
    let root = fs.btree.cache.roots.roots_b.get(btree.0 as usize).map_or(0, |r| r.get());
    if root != 0 {
        prt!(out, "{}", root & ((1 << c::BTREE_ROOT_LEVEL_BITS) - 1));
    } else {
        prt!(out, "(unknown)");
    }
    out.newline();

    bkey_val_to_text(out, Some(fs), k);
}

pub fn bch2_btree_pos_to_text(out: &mut Printbuf, fs: &Fs, b: &BtreeNode) {
    let k = b.key_peek();
    btree_pos_to_text(out, fs, Btree(b.c.btree_id.get().into()), b.c.level.get().into(), BkeySC::from(&*k))
}

#[cold]
pub fn btree_node_to_text(out: &mut Printbuf, fs: &Fs, b: &BtreeNode, contents: &NodeContents) {
    let mut stats = BsetStats::default();
    b.keys(contents).btree_keys_stats(&mut stats);

    let data = contents.data();
    prt!(out, "l {} {} - {}:\n", b.c.level.get(), data.min_key, data.max_key);
    prt!(out, "    ptrs: ");
    val_to_text(out, Some(fs), BkeySC::from(b.key(contents)));
    out.newline();

    prt!(out, "    format: ");
    contents.format.to_text(out);

    let nr = &contents.nr;
    prt!(out, "    unpack fn len: {}\n", contents.unpack_fn_len);
    prt!(out, "    bytes used {}/{} ({}% full)\n",
         usize::from(nr.live_u64s) * size_of::<u64>(),
         b.buf_bytes() - size_of::<Head<c::btree_node>>(),
         usize::from(nr.live_u64s) * 100 / fs.btree_max_u64s());
    prt!(out, "    sib u64s: {}, {} (merge threshold {})\n",
         contents.sib_u64s[0].get(), contents.sib_u64s[1].get(), fs.btree.foreground_merge_threshold);
    prt!(out, "    nr packed keys {}\n", nr.packed_keys);
    prt!(out, "    nr unpacked keys {}\n", nr.unpacked_keys);
    prt!(out, "    floats {}\n", stats.floats);
    prt!(out, "    failed unpacked {}\n", stats.failed);
}

fn prt_btree_cache_line(out: &mut Printbuf, fs: &Fs, label: &str, nr: usize) {
    prt!(out, "{label}\t{} ({nr})\n", HumanReadable(nr as u64 * u64::from(fs.opts.btree_node_size.get())));
}

impl ToText for c::bch_fs_btree_cache {
    #[cold]
    fn to_text(&self, out: &mut Printbuf) {
        let fs = self.container().container();
        // racy: read without bc->lock
        let l = unsafe { &*self.lock.data_ptr() };
        let live = &self.lock.live_nr;

        if out.nr_tabstops() == 0 {
            out.tabstop_push(32);
        }

        prt_btree_cache_line(out, fs, "live:",      live[0].nr_clean.get() + live[0].nr_dirty.get());
        prt_btree_cache_line(out, fs, "pinned:",    live[1].nr_clean.get() + live[1].nr_dirty.get());
        prt_btree_cache_line(out, fs, "vmalloc:",   l.nr_vmalloc);
        prt_btree_cache_line(out, fs, "reserve:",   self.lock.nr_reserve.get());
        prt_btree_cache_line(out, fs, "freeable:",  self.lock.nr_freeable.get());
        prt_btree_cache_line(out, fs, "dirty:",     live[0].nr_dirty.get() + live[1].nr_dirty.get());
        prt_btree_cache_line(out, fs, "in flight:", self.nr_in_flight.load(Relaxed) as usize);
        prt!(out, "cannibalize lock:\t{}\n", if self.alloc_lock.load(Relaxed).is_null() { "not held" } else { "held" });
        out.newline();

        for (i, &nr) in l.nr_by_btree.iter().enumerate() {
            prt!(out, "{}\t{} ({nr})\n", Btree(i as u32),
                 HumanReadable(nr as u64 * u64::from(fs.opts.btree_node_size.get())));
        }

        out.newline();
        prt!(out, "counters since mount:\n");
        prt!(out, "requested:\t{}\n", l.nr_requested);
        prt!(out, "freed:\t{}\n", l.nr_freed);
        prt!(out, "self reclaim:\t{}\n", self.nr_self_reclaim.load(Relaxed));
        prt!(out, "not freed:\n");

        for (i, &nr) in l.not_freed.iter().enumerate() {
            prt!(out, "  {}\t{nr}\n", c::BtreeCacheNotFreedReason::from_bits(i as u32));
        }
    }
}

// ── For init.c ───────────────────────────────────────────────────────────

