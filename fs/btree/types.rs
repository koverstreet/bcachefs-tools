// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::btree::bkey::{BkeyI, BKEY_U64S};
use crate::btree::trans_attempt::TransBkey;
use crate::data::keylist::PaddedKey;
use crate::fs::Fs;
use crate::btree::node::{sort::SortRun, BtreeNode};
use crate::c;
use crate::cstructs::c as cs;
#[cfg(feature = "std")]
use crate::errcode::bch_errcode;
use crate::errcode::BchError;
use crate::locking::{ExclusiveGuard, Locked, LockedBy, Memalloc, PF_MEMALLOC_NOIO, PF_MEMALLOC_NORECLAIM,
                     PF_MEMALLOC_NOWARN};
use crate::util::alloc::{DArrayLoan, KVVec, Kmalloc};
use crate::util::bio::{BioSet, Either};
use crate::util::late::Late;
use crate::util::Printbuf;
use crate::util::mempool::Mempool;
use crate::util::rcu::Srcu;
use crate::util::workqueue::{Workqueue, WqFlags};
use crate::types::{CacheAligned, DArray, NeBytes, RustOnly, bits_to_longs, c_default, rust_only};
use crate::btree::node_scan::NodeScan;
use crate::btree::path::BtreePath;
use crate::new_lock;
use crate::util::projections::Volatile;
use crate::util::ffi::{c_char, Opaque};
use crate::util::percpu::PerCpu;
use crate::util::shrinker::Shrinker;
use core::cell::Cell;
use core::ffi::{c_int, c_long, c_uint, c_ulong};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicIsize, AtomicU8, AtomicUsize};
use core::sync::atomic::Ordering::Relaxed;
use cstruct_macros::{atomic_bitfield, c_name, bitfield, c_const, c_enum, c_typedef, c_xmacro, placed, CStruct, Init,
                     Late, TypeInfo};
use pin_init::{pin_data, PinInit};
use nestify::nest;
#[cfg(feature = "std")]
use std::str::FromStr;
use crate::format::{Bpos, Btree, BtreeFlags, BkeyType};

/* Btree nodes: */

#[repr(C)]
#[derive(CStruct)]
pub struct btree_write {
    pub journal: cs::journal_entry_pin,
}

#[repr(C)]
#[derive(CStruct)]
pub struct btree_bkey_cached_common {
    #[c_lock("struct six_lock lock")]
    pub lock: Locked<cs::six_lock>,
    pub level: u8,
    pub btree_id: u8,
    pub cached: bool,
}

/*
 * Membership state of a struct btree in the btree node cache.
 *
 * Stored in b->cache_state and maintained by btree_node_transition_state().
 * See DOC(btree-node-cache), in btree/cache/mod.rs, for the state machine and
 * the bookkeeping each state implies.
 */
c_enum! {
    #[closed]
    #[prefix(BTREE_NODE_CACHE_)]
    pub enum btree_node_cache_state: u32 {
        NONE,           /* off all lists; not in cache (kzalloc default) */
        FREED,          /* on bc->freed_{pcpu,nonpcpu}; no data buffer */
        FREEABLE,       /* on bc->freeable; has data; not hashed */
        CLEAN,          /* on bc->live[pinned].clean; hashed; has data */
        DIRTY,          /* on bc->live[pinned].dirty; hashed; has data */
    }
}

c_enum! {
    #[closed]
    pub enum btree_node_sibling: u32 {
        btree_prev_sib,
        btree_next_sib,
    }
}

/* Btree cache: */

c_enum! {
    #[closed]
    #[prefix(BCH_BTREE_CACHE_NOT_FREED_)]
    #[nr(BCH_BTREE_CACHE_NOT_FREED_REASONS_NR)]
    #[c_list(BCH_BTREE_CACHE_NOT_FREED_REASONS)]
    pub enum bch_btree_cache_not_freed_reasons: u32 {
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
}

/// The node cache's lists: intrusive, one of list.rs's exceptions - a node
/// moves between them on every state change, in place, and is never freed
/// while the fs runs.
pub type BtreeNodeList = crate::list!(BtreeNode, list);

#[repr(C)]
#[derive(CStruct)]
pub struct btree_cache_list {
    #[c("struct list_head clean")]
    pub clean: BtreeNodeList,
    #[c("struct list_head dirty")]
    pub dirty: BtreeNodeList,
}
c_default!(btree_cache_list);

/// live[idx]'s shrinker: not bc->lock's, as the lists are - the shrinker
/// runs without it.
#[placed(cs::bch_fs_btree_cache.shrinkers[], index = idx: u32)]
#[pin_data]
#[repr(C)]
#[derive(CStruct, Init)]
pub struct btree_cache_shrinker {
    #[c("struct shrinker *shrink")]
    pub(crate) shrink: Shrinker<Self>,
}

/* On disk root - see async splits: */
#[repr(C)]
#[derive(CStruct)]
pub struct btree_root {
    #[c_anon("struct { struct bkey_i key; u64 key_pad[BKEY_BTREE_PTR_VAL_U64s_MAX]; }")]
    pub key: PaddedKey<{ BKEY_U64S + cs::BKEY_BTREE_PTR_VAL_U64s_MAX }>,
    pub level: u8,
    pub alive: u8,
}
c_default!(btree_root);

/// root_lock's: each btree's root as the journal and the clean section
/// record it - key, level, alive. The known btrees', and the rest's, which
/// early journal replay adds as it finds them: the slots only move then.
#[repr(C)]
#[derive(CStruct)]
pub struct BtreeRoots {
    pub roots_known: [cs::btree_root; cs::BTREE_ID_NR as usize],
    pub roots_extra: DArray<cs::btree_root>,
}

impl BtreeRoots {
    /// Btree @id's: a known btree's, or past them, roots_extra's -
    /// bch2_btree_id_root(). None for one past those.
    pub fn get(&self, id: u32) -> Option<&cs::btree_root> {
        let nr_known = Btree::NR;
        if id < nr_known {
            return Some(&self.roots_known[id as usize]);
        }
        let idx = (id - nr_known) as usize;
        // SAFETY: roots_extra's nr are its elements
        (idx < self.roots_extra.nr).then(|| unsafe { &*self.roots_extra.data.add(idx) })
    }

    pub fn get_mut(&mut self, id: u32) -> Option<&mut cs::btree_root> {
        let nr_known = Btree::NR;
        if id < nr_known {
            return Some(&mut self.roots_known[id as usize]);
        }
        let idx = (id - nr_known) as usize;
        // SAFETY: as get()'s
        (idx < self.roots_extra.nr).then(|| unsafe { &mut *self.roots_extra.data.add(idx) })
    }
}

/// root_lock, held: the roots, and what it publishes of them.
pub type BtreeRootsGuard<'a> = ExclusiveGuard<'a, cs::mutex, BtreeRoots, BtreeRootsLive>;

impl BtreeRootsGuard<'_> {
    /// Slots up to btree @id's, empty, and how many there are published: for
    /// a btree past the known ones that the journal has a root for.
    pub fn grow_to(&mut self, id: u32) -> Result<(), BchError> {
        {
            let mut extra = unsafe { DArrayLoan::new(&mut self.roots_extra) };
            while id >= extra.len() as u32 + Btree::NR {
                extra.push(cs::btree_root::default())?;
            }
        }
        let nr = self.roots_extra.nr as u32;
        self.published().roots_nr_extra.set(nr);
        Ok(())
    }
}

c_const! {
    /* A packed root's level, in its node pointer's low bits */
    #[c_int]
    pub const BTREE_ROOT_LEVEL_BITS: u32 = 3;
}

crate::published! {
    /// The roots as they're read without root_lock, by btree id: the root
    /// node, packed with its level - so btree_path_lock_root() reads both in
    /// one load, without a miss into the node - and the error reading it
    /// left; and how many btrees there are past the known ones. Fixed at
    /// BTREE_ID_NR_MAX, so nothing here moves when roots_extra grows.
    #[derive(CStruct)]
    pub struct BtreeRootsLive {
        pub roots_b: [Volatile<c_ulong>; cs::BTREE_ID_NR_MAX as usize],
        pub roots_error: [Volatile<i16>; cs::BTREE_ID_NR_MAX as usize],
        pub roots_nr_extra: Volatile<u32>,
    }
}

impl BtreeRootsLive {
    /// Btree @id's root node: bch2_btree_id_root_b().
    pub fn node(&self, id: u32) -> Option<&BtreeNode> {
        let b = self.roots_b.get(id as usize)?.get() & !((1 << cs::BTREE_ROOT_LEVEL_BITS) - 1);
        // SAFETY: a root's node, live while it's the root
        unsafe { (b as *const BtreeNode).as_ref() }
    }

    /// The error reading btree @id's root left.
    pub fn error(&self, id: u32) -> c_int {
        self.roots_error.get(id as usize).map_or(0, |e| e.get().into())
    }
}

/// bc->lock's: the live lists - unpinned and pinned - the freeable and freed
/// lists, the counts, and the window of nodes pinned.
#[repr(C)]
#[derive(CStruct)]
pub struct BtreeCacheLists {
    pub live: [cs::btree_cache_list; 2],
    #[c("struct list_head freeable")]
    pub freeable: BtreeNodeList,
    #[c("struct list_head freed_pcpu")]
    pub freed_pcpu: BtreeNodeList,
    #[c("struct list_head freed_nonpcpu")]
    pub freed_nonpcpu: BtreeNodeList,

    pub nr_vmalloc: usize,
    pub nr_by_btree: [usize; cs::BTREE_ID_NR as usize],

    pub pinned_nodes_start: cs::bbpos,
    pub pinned_nodes_end: cs::bbpos,
    /* btree id mask: 0 for leaves, 1 for interior */
    pub pinned_nodes_mask: [u64; 2],

    /* shrinker stats */
    pub nr_freed: usize,
    pub nr_requested: usize,
    pub not_freed: [u64; cs::BCH_BTREE_CACHE_NOT_FREED_REASONS_NR as usize],
}

/// bc->lock: its holder never enters reclaim, which would take it again
/// through the node cache's shrinker. NOIO too, for the kernel's own
/// allocations, which don't know NORECLAIM.
pub(crate) type BtreeCacheMutex = Memalloc<cs::mutex, { PF_MEMALLOC_NOIO | PF_MEMALLOC_NORECLAIM | PF_MEMALLOC_NOWARN }>;

/// bc->lock, held.
pub type BtreeCacheGuard<'a> = ExclusiveGuard<'a, BtreeCacheMutex, BtreeCacheLists, BtreeCacheLive>;

crate::published! {
    /// A live list's nodes, clean and dirty: changed with its list, read
    /// without bc->lock - the shrinker's count, the write throttle.
    #[derive(CStruct)]
    pub struct BtreeCacheLiveNr {
        pub nr_clean: Volatile<usize>,
        pub nr_dirty: Volatile<usize>,
    }
}

crate::published! {
    #[derive(CStruct)]
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

#[placed(cs::bch_fs_btree.cache)]
#[pin_data]
#[repr(C)]
#[derive(CStruct, TypeInfo, Init)]
pub struct bch_fs_btree_cache {
    #[pin]
    #[init(crate::new_lock!(<- ::pin_init::init_zeroed(), ::pin_init::zeroed(), "&bc->root_lock"))]
    #[c_lock("struct mutex root_lock")]
    pub roots: Locked<cs::mutex, BtreeRoots, BtreeRootsLive>,

    #[pin]
    #[init(RustOnly::new(::pin_init::init_zeroed()))]
    pub(crate) table: rust_only!(crate::btree::cache::NodeTable),
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
    #[c_lock("struct mutex lock")]
    pub lock: Locked<BtreeCacheMutex, BtreeCacheLists, BtreeCacheLive>,
    #[pin]
    pub shrinkers: [cs::btree_cache_shrinker; 2],

    /*
     * Every struct btree the cache has, whatever its state: to walk them all
     * without bc->lock. A node keeps its slot until it's freed, at exit, so a
     * walk can drop RCU and pick up from where it was. Node scan's buffers
     * aren't the cache's, and are freed while we run: they're not on it.
     */
    #[pin]
    #[init(RustOnly::new(::pin_init::init_zeroed()))]
    pub nodes: rust_only!(cs::fast_list),

    /* Number of nodes with BTREE_NODE_write_in_flight set. */
    #[pin]
    #[init(RustOnly::new(AtomicIsize::new(0)))]
    pub nr_in_flight: rust_only!(AtomicIsize),
    pub nr_in_flight_inner: AtomicIsize,
    pub nr_in_flight_wait: cs::closure_waitlist,
    pub __should_throttle_align: CacheAligned,
    #[c("bool should_throttle")]
    pub should_throttle: AtomicBool,

    /*
     * Times the allocator hit the memory-pressure self reclaim path:
     * journal replay watches for this going nonzero to switch off the
     * sorted-order fastpath, which holds every journal pin until replay
     * finishes - pins must be released for reclaim to clean btree nodes.
     */
    #[pin]
    #[init(RustOnly::new(AtomicIsize::new(0)))]
    pub nr_self_reclaim: rust_only!(AtomicIsize),

    /*
     * If we need to allocate memory for a new btree node and that
     * allocation fails, we can cannibalize another node in the btree cache
     * to satisfy the allocation - lock to guarantee only one thread does
     * this at a time:
     */
    #[pin]
    #[init(RustOnly::new(core::sync::atomic::AtomicPtr::new(core::ptr::null_mut())))]
    pub alloc_lock: rust_only!(core::sync::atomic::AtomicPtr<cs::task_struct>),
    #[pin]
    #[init(RustOnly::new(::pin_init::init_zeroed()))]
    pub alloc_wait: rust_only!(cs::closure_waitlist),
}

/* Iterator, update, and trigger flags: */

c_enum! {
    #[closed]
    #[prefix(BTREE_ITER_FLAG_BIT_)]
    #[mask(btree_iter_update_trigger_flags: u32, prefix = BTREE_ITER_)]
    pub enum _: u32 {
        slots,
        prev,
        intent,
        prefetch,
        is_extents,
        not_extents,
        cached,
        with_key_cache,
        with_journal,
        snapshot_field,
        all_snapshots,
        filter_snapshots,
        nofilter_whiteouts,
        nopreserve,
        nofill,
        cached_nofill,
        key_cache_fill,
        committed,
        trim,
        read_errors,

        #[mask_prefix(BTREE_UPDATE_)]
        internal_snapshot_node,
        nojournal,
        key_cache_reclaim,
        overwrite_triggered,

        /*
         * BTREE_TRIGGER_norun - don't run triggers at all
         *
         * BTREE_TRIGGER_transactional - we're running transactional triggers as part of
         * a transaction commit: triggers may generate new updates
         *
         * BTREE_TRIGGER_atomic - we're running atomic triggers during a transaction
         * commit: we have our journal reservation, we're holding btree node write
         * locks, and we know the transaction is going to commit (returning an error
         * here is a fatal error, causing us to go emergency read-only)
         *
         * BTREE_TRIGGER_gc - we're in gc/fsck: running triggers to recalculate e.g. disk usage
         *
         * transactional, atomic and gc are the phase a trigger runs in, and every
         * caller passes exactly one - commit's three passes, gc's check, interior
         * updates. Triggers rely on it: the reflink_p trigger's transactional and
         * gc passes share one walk position, right only because one of them runs.
         * XXX: a bitmask can't say "exactly one"; the phase wants to be in the
         * trigger's type, dependent typing over the flags.
         *
         * BTREE_TRIGGER_insert - @new is entering the btree
         * BTREE_TRIGGER_overwrite - @old is leaving the btree
         */
        #[mask_prefix(BTREE_TRIGGER_)]
        norun,
        transactional,
        atomic,
        gc,
        /* written with its prefix: bitflags' insert() is the mask's */
        BTREE_ITER_FLAG_BIT_insert,
        overwrite,
        is_discard,
        set_needs_reconcile_done,
    }
}

#[repr(C)]
#[derive(Default, CStruct)]
pub struct btree_trigger_op {
    pub btree: Btree,
    pub level: c_uint,
    pub old: cs::bkey_s_c,
    pub new: cs::bkey_s,
    pub new_buf_u64s: c_uint,
    pub flags: cs::btree_iter_update_trigger_flags,
}

// Btree paths and iterators:

/*
 * btree_iter: the high level btree iterator API, iterates over keys.
 * btree_path: the low level path to a btree node, holds locks.
 *
 * Multiple iterators can share the same btree_path via refcounting.
 */
nest! {
    /// A btree iterator, in transaction 't - C's struct btree_iter. Its
    /// position, and the key a peek returned - whose header is in the
    /// iterator, k - change only through &mut self, iter.rs's methods; through
    /// &self, only key_cache_path, which an update sets up. Hence its Cell.
    #[repr(C)]
    #[c_name(btree_iter)]
    #[derive(CStruct)]
    pub struct BtreeIter<'t> {
        pub(in crate::btree) trans: *mut BtreeTrans<'static>,
        pub(in crate::btree) path: cs::PathIdx,
        pub(in crate::btree) update_path: cs::PathIdx,
        #[c("btree_path_idx_t key_cache_path")]
        pub(in crate::btree) key_cache_path: core::cell::Cell<cs::PathIdx>,

        #[c_bitfield]
        #>[bitfield(u8)]
        pub(in crate::btree) bits: pub struct btree_iter_bits {
            #[bits(8)] pub(self) btree_id: Btree,
        },
        pub(in crate::btree) min_depth: u8,

        /* btree_iter_copy starts here: */
        pub(in crate::btree) flags: u32,

        /* When we're filtering by snapshot, the snapshot ID we're looking for: */
        pub(in crate::btree) snapshot: c_uint,

        pub(in crate::btree) pos: Bpos,
        /*
         * Current unpacked key - so that advancing the iterator can
         * correctly advance pos.
         */
        pub(in crate::btree) k: cs::bkey,

        /* BTREE_ITER_with_journal: */
        pub(in crate::btree) journal_idx: usize,
        /* C's TRACK_PATH_ALLOCATED: see btree_path */
        #[cfg(any(CONFIG_BCACHEFS_LOCK_TIME_STATS, CONFIG_BCACHEFS_DEBUG))]
        pub(in crate::btree) ip_allocated: c_ulong,

        /* It mustn't outlive the transaction its paths are in */
        #[c_anon("")]
        _trans: core::marker::PhantomData<&'t BtreeTrans<'t>>,
    }
}

impl BtreeIter<'_> {
    /// Set when it's initialized.
    pub fn btree(&self) -> Btree {
        self.bits.btree_id()
    }

    pub(in crate::btree) fn set_btree(&mut self, btree: Btree) {
        self.bits.set_btree_id(btree);
    }
}

/// C's name, as FFI declarations spell it: *mut c::btree_iter.
pub type btree_iter = BtreeIter<'static>;


/* Key cache: */

c_const! {
    #[c_int]
    pub const BKEY_CACHED_ACCESSED: u32 = 0;
    #[c_int]
    pub const BKEY_CACHED_DIRTY: u32 = 1;
    #[c_int]
    pub const BKEY_CACHED_IMMEDIATE_FLUSH: u32 = 2;
}

#[repr(C)]
#[derive(CStruct)]
pub struct bkey_cached {
    pub c: cs::btree_bkey_cached_common,

    #[c("unsigned long flags")]
    pub flags: AtomicUsize,
    pub u64s: u16,
    pub key: cs::bkey_cached_key,

    pub keys_idx: c_uint,

    pub journal: cs::journal_entry_pin,
    pub seq: u64,

    pub k: *mut cs::bkey_i,
    pub rcu: cs::rcu_head,
}
c_default!(bkey_cached);

impl bkey_cached {
    /* C's test_bit()s and set_bit()s */
    fn flag(&self, bit: u32) -> bool {
        self.flags.load(Relaxed) & (1 << bit) != 0
    }

    pub fn dirty(&self) -> bool {
        self.flag(cs::BKEY_CACHED_DIRTY)
    }

    pub fn accessed(&self) -> bool {
        self.flag(cs::BKEY_CACHED_ACCESSED)
    }

    pub fn immediate_flush(&self) -> bool {
        self.flag(cs::BKEY_CACHED_IMMEDIATE_FLUSH)
    }

    pub fn set_flag(&self, bit: u32) {
        self.flags.fetch_or(1 << bit, Relaxed);
    }

    pub fn clear_flag(&self, bit: u32) {
        self.flags.fetch_and(!(1 << bit), Relaxed);
    }
}

/* Transaction types: */
nest! {
    #[repr(C)]
    #[derive(Clone, Copy, CStruct)]
    pub struct btree_insert_entry {
        pub flags: c_uint,
        pub sort_order: u8,
        pub bkey_type: u8,
        #[c_bitfield]
        #>[bitfield(u16, ty = u8)]
        pub bits: pub struct btree_insert_entry_bits {
            #[bits(8)] pub btree_id: Btree,
            level:                     u3,
            cached:                    bool,
            insert_trigger_run:        bool,
            overwrite_trigger_run:     bool,
            key_cache_already_flushed: bool,
            key_cache_flushing:        bool,
        },
        /*
         * @old_k may be a key from the journal or the key cache;
         * @old_btree_u64s always refers to the size of the key being
         * overwritten in the btree:
         */
        pub old_btree_u64s: u8,
        k_buf_u64s: u8,
        pub path: cs::PathIdx,
        k: *mut cs::bkey_i,
        /* key being overwritten: */
        pub old_k: cs::bkey,
        pub old_v: *const cs::bch_val,
        pub ip_allocated: c_ulong,
    }
}

c_const! {
    /* Number of btree paths we preallocate, usually enough */
    #[c_int]
    pub const BTREE_ITER_INITIAL: u32 = 64;

    /*
     * Lmiit for btree_trans_too_many_iters(); this is enough that almost all code
     * paths should run inside this limit, and if they don't it usually indicates a
     * bug (leaking/duplicated btree paths).
     *
     * exception: some fsck paths
     *
     * bugs with excessive path usage seem to have possibly been eliminated now, so
     * we might consider eliminating this (and btree_trans_too_many_iter()) at some
     * point.
     */
    #[c_int]
    pub const BTREE_ITER_NORMAL_LIMIT: u32 = 256;

    /* never exceed limit */
    pub const BTREE_ITER_MAX: u32 = 1 << 10;
}

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct btree_trans_commit_hook {
    pub fn_: Option<unsafe extern "C" fn(*mut BtreeTrans<'_>, *mut cs::btree_trans_commit_hook) -> c_int>,
    pub next: *mut cs::btree_trans_commit_hook,
}
c_default!(btree_trans_commit_hook);

c_const! {
    pub const BTREE_TRANS_MEM_MAX: u32 = 1 << 16;

    pub const BTREE_TRANS_MAX_LOCK_HOLD_TIME_NS: c_long = cs::NSEC_PER_MSEC as c_long;
}

#[repr(C)]
#[derive(Default, CStruct)]
pub struct btree_trans_paths {
    pub nr_paths: c_ulong,
    pub paths: [BtreePath; 0],
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct trans_kmalloc_trace {
    pub ip: c_ulong,
    pub bytes: usize,
}

c_typedef! {
    #[c("DARRAY(struct trans_kmalloc_trace) darray_trans_kmalloc_trace")]
    pub type darray_trans_kmalloc_trace = KVVec<cs::trans_kmalloc_trace>;
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct btree_trans_subbuf {
    pub base: u16,
    pub u64s: u16,
    pub size: u16,
}

/// The transaction's memory: a vector of u64s, bump-allocated from - what it
/// hands out is in use, by reference - and cleared on every begin; so it's
/// reallocated only by begin, when nothing it handed out is used again.
#[repr(C)]
#[c_name(trans_mem)]
#[derive(CStruct, Default)]
pub struct TransMem {
    #[c_anon("struct { u32 nr, size; u64 *data; }")]
    pub(crate) buf:                    crate::util::alloc::Vec<u64, Kmalloc, u32>,
    /* bytes: what the next begin grows it to, restarted for room */
    pub(crate) realloc_bytes_required: u32,
    /* a mempool element - kmalloc()'d, but the pool's to have back */
    pub(crate) used_mempool:           bool,
}

/*
 * Transaction context for btree operations.
 *
 * Holds iterators/paths, pending updates, locks, and a memory arena for a
 * single logical btree operation.  On lock contention or memory pressure,
 * the transaction restarts: releases all locks, resets state, and retries
 * (via lockrestart_do() or similar retry loops).
 *
 *  - mem/mem_top: bump allocator, invalidated on every restart
 *  - Paths kept sorted in lock order to prevent deadlocks
 *  - SRCU read lock protects btree node memory from being freed;
 *    released periodically to avoid stalling reclaim
 */
#[bitfield(u64, repr = NeBytes<6>, from = NeBytes::<6>::from_u64, into = NeBytes::<6>::to_u64)]
pub struct btree_trans_bits {
    lock_may_not_fail:              bool,
    locked:                         bool,
    migrate_disabled:               bool,
    write_locked:                   bool,
    srcu_held:                      bool,
    /*
     * IO was submitted under the srcu read lock (submit_bio() can block for
     * an unbounded time): suppress the next bch2_trans_unlock_long() "held
     * too long" warning. Scoped to one locked attempt — cleared on relock /
     * bch2_trans_begin().
     */
    srcu_io_submitted:              bool,
    btree_cache_cannibalize_locked: bool,
    pf_memalloc_noio:               bool,
    in_traverse_all:                bool,
    paths_sorted:                   bool,
    memory_allocation_failure:      bool,
    journal_transaction_names:      bool,
    journal_replay_not_finished:    bool,
    pub(self) notrace_relock_fail:  bool,
    /*
     * Exempt bch2_trans_begin() from the dropped-updates warning. Set
     * around a nested transaction - one that grabs trans->restart_count,
     * does work whose inner commits discard the outer's queued updates, and
     * returns trans_was_restarted() (e.g. fsck counting i_sectors, which
     * spans too many extents to be a single transaction). The begin can't
     * see the restart that's about to be returned, so the caller vouches
     * for it here. Set it with CLASS(trans_may_drop_updates), which
     * restores it at the end of the scope.
     */
    begin_may_drop_updates:         bool,
    has_interior_updates:           bool,
    /* Rust's: the path table is borrowed (btree/path.rs's Paths) */
    paths_borrowed:                 bool,
    __pad17:                        u15,
    #[bits(16)] pub restarted: cs::bch_errcode,
    __pad:                          u16,
}

/*
 * Transaction context for btree operations.
 *
 * Holds iterators/paths, pending updates, locks, and a memory arena for a
 * single logical btree operation.  On lock contention or memory pressure,
 * the transaction restarts: releases all locks, resets state, and retries
 * (via lockrestart_do() or similar retry loops).
 *
 *  - mem/mem_top: bump allocator, invalidated on every restart
 *  - Paths kept sorted in lock order to prevent deadlocks
 *  - SRCU read lock protects btree node memory from being freed;
 *    released periodically to avoid stalling reclaim
 */
#[repr(C)]
#[c_name(btree_trans)]
#[derive(CStruct)]
pub struct BtreeTrans<'f> {
    #[c("struct bch_fs *c")]
    pub c: &'f Fs,

    pub paths_allocated: Cell<*mut c_ulong>,
    pub paths: Cell<*mut BtreePath>,
    pub sorted: Cell<*mut cs::PathIdx>,
    pub updates: Cell<*mut cs::btree_insert_entry>,

    /* bump allocator, invalidated on transaction restart */
    #[c("struct trans_mem mem")]
    pub mem: Cell<TransMem>,
    #[cfg(CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE)]
    #[c("darray_trans_kmalloc_trace trans_kmalloc_trace")]
    pub trans_kmalloc_trace: Cell<cs::darray_trans_kmalloc_trace>,

    pub nr_sorted: Cell<cs::PathIdx>,
    pub nr_paths: Cell<cs::PathIdx>,
    pub nr_paths_max: Cell<cs::PathIdx>,
    pub nr_updates: Cell<cs::PathIdx>,
    pub shard_cpu: Cell<i16>,
    pub fn_idx: Cell<u8>,
    #[c("u8 lock_must_abort")]
    pub lock_must_abort: AtomicU8,
    #[c_bitfield]
    pub bits: Cell<btree_trans_bits>,
    pub restart_count: Cell<u32>,
    #[cfg(CONFIG_BCACHEFS_INJECT_TRANSACTION_RESTARTS)]
    pub restart_count_this_trans: Cell<u32>,
    /*
     * Incremented on every successful (non-empty) commit; for detecting
     * that cached state derived from btree reads may be stale:
     */
    pub commit_count: Cell<u32>,

    pub last_begin_time: Cell<u64>,
    pub last_begin_ip: Cell<c_ulong>,
    pub last_restarted_ip: Cell<c_ulong>,
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    #[c("bch_stacktrace last_restarted_trace")]
    pub last_restarted_trace: Cell<cs::bch_stacktrace>,
    pub last_unlock_ip: Cell<c_ulong>,
    pub srcu_lock_time: Cell<c_ulong>,
    pub srcu_idx: Cell<c_int>,
    pub locking_root_id: Cell<Btree>,

    pub locking_hash_val: Cell<u64>,
    pub locking: Cell<*mut cs::btree_bkey_cached_common>,
    /*
     * Snapshot of locking->{btree}.hash_val at lock-attempt time, used by
     * bch2_six_check_for_deadlock() to detect that the node identity
     * rotated while we were about to sleep on it. 0 for cached entries.
     */
    #[c("struct six_lock_waiter locking_wait")]
    pub locking_wait: cs::six_lock_waiter,

    /*
     * btree node writes issued in this trans's context are queued here
     * (singly linked via bi_next) instead of being submitted directly —
     * no block layer work happens while we hold btree node locks.
     * Submitted when the trans unlocks, and before waiting on btree
     * node IO (see bch2_btree_node_wait_on_write()).
     */
    pub queued_write_bios: Cell<*mut cs::bio>,

    pub fn_: Cell<*const c_char>,

    /* update path: */
    pub journal_entries: Cell<cs::btree_trans_subbuf>,
    pub accounting: Cell<cs::btree_trans_subbuf>,

    pub hooks: Cell<*mut cs::btree_trans_commit_hook>,
    pub journal_pin: Cell<*mut cs::journal_entry_pin>,

    pub journal_res: Cell<cs::journal_res>,
    /*
     * Input for BCH_TRANS_COMMIT_no_journal_res: with no reservation to
     * derive a seq from, the caller names the journal entry the btree node
     * write has to pin.
     */
    pub journal_seq_to_pin: Cell<u64>,
    pub journal_seq: Cell<*mut u64>,
    pub disk_res: Cell<*mut cs::disk_reservation>,
    pub flush: Cell<*mut cs::closure>,

    pub fs_usage_delta: Cell<cs::bch_fs_usage_base>,

    pub journal_u64s: c_uint,
    pub extra_journal_u64s: Cell<u32>,

    /*
     * Space a trigger needs on top of @disk_res, charged at commit.
     * Charge sites may disagree on the count, so keep the max: erring
     * high only over-reserves.
     */
    pub extra_disk_res: Cell<u64>,
    pub extra_disk_res_replicas: Cell<u8>,

    #[c_anon("struct { struct bkey_i btree_path_down; u64 btree_path_down_pad[BKEY_BTREE_PTR_VAL_U64s_MAX]; }")]
    pub btree_path_down: LockedBy<PaddedKey<{ BKEY_U64S + cs::BKEY_BTREE_PTR_VAL_U64s_MAX }>>,

    #[cfg(CONFIG_DEBUG_LOCK_ALLOC)]
    #[c("struct lockdep_map dep_map")]
    pub dep_map: crate::locking::LockdepMap,
    /* Entries before this are zeroed out on every bch2_trans_get() call */

    pub list: cs::list_head,
    #[c("struct closure ref")]
    pub ref_: Opaque<cs::closure>,
    pub rcu: cs::rcu_head,

    /* the first path table: Paths borrows into it */
    #[c("unsigned long _paths_allocated[BITS_TO_LONGS(BTREE_ITER_INITIAL)]")]
    pub _paths_allocated: Opaque<[c_ulong; bits_to_longs(cs::BTREE_ITER_INITIAL as usize)]>,
    #[c("struct btree_trans_paths trans_paths")]
    pub trans_paths: Opaque<cs::btree_trans_paths>,
    #[c("struct btree_path _paths[BTREE_ITER_INITIAL]")]
    pub _paths: Opaque<[BtreePath; cs::BTREE_ITER_INITIAL as usize]>,
    #[c("btree_path_idx_t _sorted[BTREE_ITER_INITIAL + 4]")]
    pub _sorted: Opaque<[cs::PathIdx; cs::BTREE_ITER_INITIAL as usize + 4]>,
    #[c("struct btree_insert_entry _updates[BTREE_ITER_INITIAL]")]
    pub _updates: Opaque<[cs::btree_insert_entry; cs::BTREE_ITER_INITIAL as usize]>,
}

impl BtreeTrans<'_> {
    pub fn notrace_relock_fail(&self) -> bool {
        self.bits.get().notrace_relock_fail()
    }

    /// Relock failures after IO it submitted are expected, not counted.
    pub fn set_notrace_relock_fail(&self) {
        self.bits.set(self.bits.get().with_notrace_relock_fail(true))
    }

    /// For the next attempt: bch2_trans_begin()'s, after a restart.
    pub fn clear_notrace_relock_fail(&self) {
        self.bits.set(self.bits.get().with_notrace_relock_fail(false))
    }
}

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct btree_trans_buf {
    pub trans: *mut BtreeTrans<'static>,
}
c_default!(btree_trans_buf);

/// A transaction function's stats' lock's: what's kept of its biggest - its
/// memory's allocations, and its paths, as text.
#[repr(C)]
#[derive(CStruct)]
pub struct TransStatsLocked {
    #[cfg(CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE)]
    pub trans_kmalloc_trace: cs::darray_trans_kmalloc_trace,
    pub max_paths_text: Printbuf,
}

crate::published! {
    /// Its high water marks: raised under the lock, read without it - by
    /// bch2_trans_get(), sizing the transaction it gets.
    #[derive(CStruct)]
    pub struct TransStatsMax {
        pub nr_max_paths: Volatile<c_uint>,
        pub max_mem: Volatile<c_uint>,
    }
}

#[pin_data]
#[repr(C)]
#[derive(CStruct, Init)]
pub struct btree_transaction_stats {
    #[pin]
    #[init(cs::bch2_time_stats::init())]
    pub duration: cs::bch2_time_stats,
    #[pin]
    #[init(cs::bch2_time_stats::init())]
    pub lock_hold_times: cs::bch2_time_stats,
    #[pin]
    #[init(cs::bch2_time_stats::init())]
    pub lock_wait_times: cs::bch2_time_stats,
    #[pin]
    #[init(crate::new_lock!(<- ::pin_init::init_zeroed(), ::pin_init::zeroed(), "&s->lock"))]
    #[c_lock("struct mutex lock")]
    pub lock: Locked<cs::mutex, TransStatsLocked, TransStatsMax>,
}

c_const! {
    #[c_int]
    pub const BCH_TRANSACTIONS_NR: u32 = 128;
}

#[pin_data]
#[repr(C)]
#[derive(CStruct, TypeInfo, Init)]
pub struct bch_fs_btree_trans {
    #[pin]
    #[init(new_lock!((), "&c->btree.trans.lock"))]
    #[c("struct seqmutex lock")]
    pub lock: Locked<cs::seqmutex>,
    /* zeroed, it's empty: List's first push sets it up */
    pub list: cs::list_head,
    #[pin]
    #[init(Mempool::kmalloc(1, core::mem::size_of::<BtreeTrans<'_>>()))]
    #[c("mempool_t pool")]
    pub pool: Mempool,
    #[pin]
    #[init(Mempool::kmalloc(1, c::BTREE_TRANS_MEM_MAX as usize))]
    #[c("mempool_t malloc_pool")]
    pub malloc_pool: Mempool,
    #[pin]
    #[init(PerCpu::alloc(), ENOMEM_fs_other_alloc)]
    #[c("struct btree_trans_buf __percpu *bufs")]
    pub bufs: PerCpu<cs::btree_trans_buf>,

    #[pin]
    #[init(crate::new_srcu!("btree_trans_barrier"))]
    #[c("struct srcu_struct barrier")]
    pub barrier: Srcu,

    #[pin]
    #[init(::pin_init::pin_init_array_from_fn(|_| cs::btree_transaction_stats::init()))]
    pub stats: [cs::btree_transaction_stats; cs::BCH_TRANSACTIONS_NR as usize],

    #[pin]
    #[init(new_lock!(TransStatsJson { buf: Printbuf::new() }, "&c->btree.trans.stats_json_lock"))]
    #[c_lock("struct mutex stats_json_lock")]
    pub stats_json: Locked<cs::mutex, TransStatsJson>,
}

/// The transaction stats as JSON, for sysfs: made at a read's offset 0, read
/// a page at a time after.
#[repr(C)]
#[derive(CStruct)]
pub struct TransStatsJson {
    pub buf: Printbuf,
}
c_default!(bch_fs_btree_trans);

/* Btree node write types and flags: */

c_enum! {
    #[closed]
    #[prefix(BTREE_WRITE_)]
    #[nr(BTREE_WRITE_TYPE_NR)]
    #[c_name(btree_write_type)]
    pub enum BtreeWriteType: u32 {
        initial = 0,
        init_next_bset = 1,
        cache_reclaim = 2,
        journal_reclaim = 3,
        interior = 4,
    }
}

c_const! {
    pub const BTREE_WRITE_TYPE_MASK: c_ulong =
        (cs::BTREE_WRITE_TYPE_NR as c_ulong).next_power_of_two() - 1;
    #[c_int]
    pub const BTREE_WRITE_TYPE_BITS: u32 = (cs::BTREE_WRITE_TYPE_NR as u32).next_power_of_two().ilog2();
}

c_xmacro! {
    BTREE_FLAGS(x) {
        (read_in_flight),
        (read_error),
        (dirty),
        (need_write),
        (write_blocked),
        (will_make_reachable),
        (noevict),
        (write_idx),
        (accessed),
        (write_in_flight),
        (write_in_flight_inner),
        (just_written),
        (dying),
        (fake),
        (need_rewrite),
        (need_rewrite_error),
        (need_rewrite_ptr_written_zero),
        (never_write),
        (pinned),
        (permanent),
    }
}

c_enum! {
    #[closed]
    pub enum btree_flags: u32 {
        /* First bits for btree node write type */
        BTREE_NODE_FLAGS_START = cs::BTREE_WRITE_TYPE_BITS - 1,
        BTREE_FLAGS(flag) => concat(BTREE_NODE_, flag),
    }
}

atomic_bitfield! {
    /// struct btree's flags: the pending write's type, then BTREE_FLAGS().
    pub struct BtreeNodeFlags: u32 in usize {
        #[bits(3)] write_type: BtreeWriteType,
        BTREE_FLAGS(flag) => flag: bool,
    }
}

// Where C's set_bit()s and masks have them, until btree/write.c, commit.c
// and interior.c are Rust
macro_rules! btree_node_flags_at_c_bits {
    ([] $(($f:ident)),* $(,)?) => { ::paste::paste! { $(
        const _: () = assert!(BtreeNodeFlags::[<$f:upper _OFFSET>] == btree_flags::[<BTREE_NODE_ $f>] as usize);
    )* } };
}
crate::cstructs::c::BTREE_FLAGS!(btree_node_flags_at_c_bits []);
const _: () = assert!(BtreeNodeFlags::WRITE_TYPE_OFFSET == 0 &&
                      BtreeNodeFlags::WRITE_TYPE_BITS == cs::BTREE_WRITE_TYPE_BITS as usize);

c_enum! {
    #[closed]
    #[prefix(BTREE_NODE_REWRITE_)]
    #[c_list(BTREE_NODE_REWRITE_REASON)]
    pub enum btree_node_rewrite_reason: u32 {
        none,
        unknown,
        error,
        ptr_written_zero,
    }
}

/* Filesystem-level btree state: */
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
#[derive(Clone, Copy, CStruct)]
pub struct btree_evicted_size {
    pub mask: u64, /* table size - 1 (power of 2) */
    pub entries: *mut u64,
}
c_default!(btree_evicted_size);

c_const! {
    #[c_int]
    pub const BTREE_EVICTED_SIZE_HASH_BITS: u32 = 48;
    pub const BTREE_EVICTED_SIZE_HASH_MASK: u64 = (1 << cs::BTREE_EVICTED_SIZE_HASH_BITS) - 1;
}

#[repr(C)]
#[derive(CStruct)]
pub struct btree_write_stats {
    pub nr: AtomicI64,
    pub bytes: AtomicI64,
}
c_default!(btree_write_stats);

#[placed(Fs.btree)]
#[pin_data]
#[repr(C)]
#[derive(CStruct, TypeInfo, Init, Late)]
#[init_args(opts: &c::bch_opts)]
pub struct bch_fs_btree {
    #[init((btree_max_u64s(opts.btree_node_size) / 3) as u16)]
    pub foreground_merge_threshold: u16,

    #[pin]
    #[init(Mempool::kvmalloc(1, opts.btree_node_size as usize), ENOMEM_fs_other_alloc)]
    #[c("mempool_t bounce_pool")]
    pub bounce_pool: Mempool,

    pub write_stats: [cs::btree_write_stats; cs::BTREE_WRITE_TYPE_NR as usize],

    #[pin]
    #[init(BioSet::new(1, true), ENOMEM_fs_other_alloc)]
    #[c("struct bio_set bio")]
    pub bio: BioSet<Either<crate::btree::read::BtreeReadBio, crate::btree::write::BtreeWriteBio>>,
    #[pin]
    #[init(fill_iter_pool(opts), ENOMEM_fs_other_alloc)]
    #[c("mempool_t fill_iter")]
    pub fill_iter: Mempool,
    #[pin]
    #[init(Workqueue::new(c"bcachefs_btree_read_complete",
                          WqFlags::HIGHPRI | WqFlags::FREEZABLE | WqFlags::MEM_RECLAIM | WqFlags::PERCPU, 512),
           ENOMEM_fs_other_alloc)]
    #[c("struct workqueue_struct *read_complete_wq")]
    pub read_complete_wq: Workqueue,
    #[pin]
    #[init(crate::util::log::ratelimit_state_init())]
    pub read_errors_soft: cs::ratelimit_state,
    #[pin]
    #[init(crate::util::log::ratelimit_state_init())]
    pub read_errors_hard: cs::ratelimit_state,

    #[late(Workqueue::new(c"bcachefs_btree_write_complete",
                          WqFlags::HIGHPRI | WqFlags::FREEZABLE | WqFlags::MEM_RECLAIM | WqFlags::PERCPU, 1),
           ENOMEM_fs_other_alloc)]
    #[c("struct workqueue_struct *write_complete_wq")]
    pub write_complete_wq: Late<Workqueue>,

    pub root_journal_res: cs::journal_entry_res,

    #[pin]
    pub cache: cs::bch_fs_btree_cache,
    pub evicted_size: cs::btree_evicted_size,
    #[pin]
    pub key_cache: cs::bch_fs_btree_key_cache,
    /*
     * One per BTREE_IS_write_buffer btree (indexed by BCH_WB_BTREE_*, see
     * bch_wb_btree_idx()). Each instance has its own intake/flushing
     * buffers and locks.
     */
    #[pin]
    pub write_buffer: [cs::bch_fs_btree_write_buffer; cs::BCH_WB_BTREE_NR as usize],
    /*
     * Per-btree flush_work runs on write_buffer_wq; once the sorted key
     * list crosses a threshold the flush parallelizes across CPUs by
     * queuing sub-shards on write_buffer_shard_wq and closure_sync()ing.
     * The two must be separate workqueues — the outer flush worker
     * blocks waiting for the inner shards to complete, so they can't
     * share a wq. WQ_MEM_RECLAIM on both because the flush sits in the
     * journal-reclaim path.
     *
     * write_buffer_wq is freezable: a flush does btree commits, and those
     * submit btree node writes from the flushing context - after a
     * hibernate snapshot that is a write the image doesn't know about.
     * The shard wq must NOT be: a flush in progress when the freeze starts
     * waits on its shards, and freeze_workqueues_busy() waits on the
     * flush. Shards are only queued by a running flush, so once the outer
     * wq is frozen no new ones appear.
     */
    #[late(Workqueue::new(c"bcachefs_wb_flush",
                          WqFlags::UNBOUND | WqFlags::MEM_RECLAIM | WqFlags::FREEZABLE, 0),
           ENOMEM_fs_other_alloc)]
    #[c("struct workqueue_struct *write_buffer_wq")]
    pub write_buffer_wq: Late<Workqueue>,
    #[late(Workqueue::new(c"bcachefs_wb_flush_shard", WqFlags::UNBOUND | WqFlags::MEM_RECLAIM, 0), ENOMEM_fs_other_alloc)]
    #[c("struct workqueue_struct *write_buffer_shard_wq")]
    pub write_buffer_shard_wq: Late<Workqueue>,
    /*
     * Sync flushers (btree_write_buffer_flush_seq) wake the per-btree
     * flush_works and then wait here for the worker to drain pins past
     * their target seq. Waked from the flush worker after drain and from
     * journal_keys_to_write_buffer_end after pin drops.
     */
    pub write_buffer_flush_wait: cs::closure_waitlist,
    #[pin]
    #[init(cs::bch_fs_btree_trans::init())]
    pub trans: cs::bch_fs_btree_trans,
    #[pin]
    #[init(cs::bch_fs_btree_reserve_cache::init())]
    pub reserve_cache: cs::bch_fs_btree_reserve_cache,
    #[pin]
    #[late]
    pub interior_updates: cs::bch_fs_btree_interior_updates,
    #[pin]
    #[init(RustOnly::new(crate::btree::async_ops::AsyncOps::init()))]
    #[late]
    pub(crate) async_ops: rust_only!(crate::btree::async_ops::AsyncOps),
    #[pin]
    #[init(RustOnly::new(new_lock!(KVVec::new())))]
    pub(crate) node_scan: rust_only!(NodeScan),
}

/// What a node of @btree_node_size bytes has room for, past its header.
pub fn btree_max_u64s(btree_node_size: u32) -> usize {
    (btree_node_size as usize - size_of::<crate::util::vstructs::Head<cs::btree_node>>()) / size_of::<u64>()
}

fn fill_iter_pool(opts: &c::bch_opts) -> impl PinInit<Mempool, BchError> {
    let btree_blocks = (opts.btree_node_size as usize >> 9) >> (opts.block_size as usize >> 9).ilog2();
    Mempool::kmalloc(1, (btree_blocks + 1) * 2 * size_of::<SortRun>())
}

/* Btree ID properties: */
c_enum! {
    #[open]
    #[c_name(btree_node_type)]
    #[prefix(BKEY_TYPE_)]
    #[nr(BKEY_TYPE_NR)]
    pub enum BtreeNodeType: u32 {
        btree,
        Btree_arms(kwd, val, ..) => kwd = (val as u32) + 1,
    }
}

impl BtreeNodeType {
    /* Type of a key in btree @id at level @level: */
    pub const fn of(level: u32, btree: Btree) -> Self {
        if level != 0 { Self::btree } else { Self(btree.0 + 1) }
    }
}

impl Btree {
    pub const fn trigger_order(self) -> u8 {
        match self {
            Btree::alloc   => u8::MAX,
            Btree::stripes => u8::MAX - 1,
            _              => self.0 as u8,
        }
    }
}

/*
 * The node types whose updates run triggers, a bit each: a leaf's node type
 * is its btree's id + 1. Every snapshotted btree runs transactional triggers,
 * so that bch2_trigger_snapshot_nr_keys() runs for its keys; and interior
 * nodes do, for their pointers.
 */
const TRANS_TRIGGERS: u64 = 1 << BtreeNodeType::btree.0 |
    crate::btrees_with(BtreeFlags::trans_triggers.union(BtreeFlags::snapshots)) << 1;
const ATOMIC_TRIGGERS: u64 = crate::btrees_with(BtreeFlags::atomic_triggers) << 1;

/// An update's node type - which may be a btree's this code doesn't know - in
/// @mask.
const fn node_type_in(bkey_type: u8, mask: u64) -> bool {
    bkey_type < 64 && mask & (1 << bkey_type) != 0
}

/// Where an update's key is, and its room: a TransBkey's - so the key is as
/// good as one is, until the attempt's over - or an update's already queued.
#[derive(Clone, Copy)]
pub(crate) struct QueuedKey {
    k:        NonNull<cs::bkey_i>,
    buf_u64s: u8,
}

impl<V: ?Sized> From<&TransBkey<'_, '_, V>> for QueuedKey {
    fn from(k: &TransBkey<'_, '_, V>) -> Self {
        QueuedKey {
            k:        NonNull::new(k.as_ptr()).expect("a TransBkey at NULL"),
            buf_u64s: k.buf_u64s.min(u8::MAX.into()) as u8,
        }
    }
}

impl btree_insert_entry {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(key: QueuedKey, path: cs::PathIdx, btree: Btree, level: u32, cached: bool,
                      flags: c_uint, ip: c_ulong) -> Self {
        let mut bits = btree_insert_entry_bits::default();
        bits.set_btree_id(btree);
        bits.set_level(level as u8);
        bits.set_cached(cached);

        btree_insert_entry {
            flags,
            sort_order:     btree.trigger_order(),
            bkey_type:      BtreeNodeType::of(level, btree).0 as u8,
            bits,
            old_btree_u64s: 0,
            k_buf_u64s:     key.buf_u64s,
            path,
            k:              key.k.as_ptr(),
            old_k:          cs::bkey::new(),
            old_v:          core::ptr::null(),
            ip_allocated:   ip,
        }
    }

    pub(crate) fn key(&self) -> &BkeyI {
        unsafe { &*BkeyI::from_raw(self.k) }
    }

    /// The key, for a trigger to change in place: it may be one the update's
    /// caller lent.
    pub(crate) fn key_ptr(&self) -> *mut cs::bkey_i {
        self.k
    }

    pub(crate) fn key_buf_u64s(&self) -> u32 {
        self.k_buf_u64s.into()
    }

    pub(crate) fn queued_key(&self) -> QueuedKey {
        QueuedKey { k: NonNull::new(self.k).expect("an update of no key"), buf_u64s: self.k_buf_u64s }
    }

    pub(crate) fn set_key(&mut self, key: QueuedKey) {
        self.k          = key.k.as_ptr();
        self.k_buf_u64s = key.buf_u64s;
    }

    pub const fn has_trans_triggers(&self) -> bool {
        node_type_in(self.bkey_type, TRANS_TRIGGERS)
    }

    pub const fn has_atomic_triggers(&self) -> bool {
        node_type_in(self.bkey_type, ATOMIC_TRIGGERS)
    }

    pub const fn has_triggers(&self) -> bool {
        node_type_in(self.bkey_type, TRANS_TRIGGERS | ATOMIC_TRIGGERS)
    }
}

impl Btree {
    /// The key types it may hold, a bit each - deleted, which every btree
    /// may, and its own: bch2_key_types_allowed[]. 0 for a btree this code
    /// doesn't know.
    pub const fn key_types(self) -> u64 {
        if self.0 < Self::NR {
            self.holds() | 1 << BkeyType::deleted.0
        } else {
            0
        }
    }

    /// Whether it's an extents btree with snapshots:
    /// btree_id_is_extents_snapshots().
    pub const fn is_extents_snapshots(self) -> bool {
        self.flags().contains(BtreeFlags::extents.union(BtreeFlags::snapshots))
    }

    /// Whether its updates go through the btree write buffer.
    pub const fn uses_write_buffer(self) -> bool {
        self.flags().contains(BtreeFlags::write_buffer)
    }

    /// Convert from raw u32. Returns None for unknown built-in btree IDs.
    pub fn from_raw(id: u32) -> Option<Self> {
        crate::BTREE_IDS_KNOWN.get(id as usize).copied()
    }
}

impl From<Btree> for u32 {
    fn from(id: Btree) -> u32 {
        id.0
    }
}

/// Get a btree ID name string.
#[cfg(feature = "std")]
pub fn btree_id_str(id: u32) -> String {
    Btree(id).to_string()
}

#[cfg(feature = "std")]
impl FromStr for Btree {
    type Err = BchError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        <Btree as crate::util::names::Names>::ENTRIES.iter().find(|&&(n, _)| n == s)
            .and_then(|&(_, v)| Btree::from_raw(v as u32))
            .ok_or(bch_errcode::BCH_ERR_EINVAL_parse_btree_id.into())
    }
}

#[cfg(feature = "std")]
impl FromStr for BkeyType {
    type Err = BchError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        crate::util::names::value_of(<Self as crate::util::names::Names>::ENTRIES, s)
            .map(|v| BkeyType(v as u32))
            .ok_or_else(|| bch_errcode::BCH_ERR_EINVAL_parse_bkey_type.into())
    }
}
