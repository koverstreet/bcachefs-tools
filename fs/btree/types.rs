// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::c;
use crate::cstructs::c as cs;
#[cfg(feature = "std")]
use crate::errcode::{bch_errcode, BchError};
use crate::types::{CacheAligned, DArray, NeBytes, bits_to_longs, c_default};
use crate::util::vstructs::Head;
use crate::util::ffi::c_char;
use core::ffi::{c_int, c_long, c_uint, c_ulong, c_void, CStr};
use core::fmt;
use cstruct_macros::{bitfield, c_const, c_enum, c_typedef, c_xmacro, CStruct, TypeInfo};
use nestify::nest;
#[cfg(feature = "std")]
use std::{ffi::CString, str::FromStr};

/* Btree nodes: */

c_const! {
    pub const MAX_BSETS: u32 = 3;
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct btree_nr_keys {
    /*
     * Amount of live metadata (i.e. size of node after a compaction) in
     * units of u64s
     */
    pub live_u64s: u16,
    pub bset_u64s: [u16; cs::MAX_BSETS as usize],

    /* live keys only: */
    pub packed_keys: u16,
    pub unpacked_keys: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct bset_tree {
    /*
     * We construct a binary tree in an array as if the array
     * started at 1, so that things line up on the same cachelines
     * better: see comments in bset.c at cacheline_to_bkey() for
     * details
     */

    /* size of the binary tree and prev array */
    pub size: u16,

    /* function of size - precalculated for to_inorder() */
    pub extra: u16,

    pub data_offset: u16,
    pub aux_data_offset: u16,
    pub end_offset: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct btree_write {
    pub journal: cs::journal_entry_pin,
}

#[repr(C)]
#[derive(Default, CStruct)]
pub struct btree_bkey_cached_common {
    pub lock: cs::six_lock,
    pub level: u8,
    pub btree_id: u8,
    pub cached: bool,
}

/*
 * Membership state of a struct btree in the btree node cache.
 *
 * Stored in b->cache_state and maintained by bch2_btree_node_transition_state().
 * See the DOC block at the top of btree/cache.c for the state machine and
 * the bookkeeping each state implies.
 */
c_enum! {
    #[closed]
    pub enum btree_node_cache_state: u32 {
        BTREE_NODE_CACHE_NONE,          /* off all lists; not in cache (kzalloc default) */
        BTREE_NODE_CACHE_FREED,         /* on bc->freed_{pcpu,nonpcpu}; no data buffer */
        BTREE_NODE_CACHE_FREEABLE,      /* on bc->freeable; has data; not hashed */
        BTREE_NODE_CACHE_CLEAN,         /* on bc->live[pinned].clean; hashed; has data */
        BTREE_NODE_CACHE_DIRTY,         /* on bc->live[pinned].dirty; hashed; has data */
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct bkey_unpack_field {
    pub byte_offset: i8,
    pub shift_right: u8, /* 64 - bits, or 64 if field has no bits in packed */
}

#[repr(C)]
#[derive(CStruct)]
pub struct btree {
    pub c: cs::btree_bkey_cached_common,

    pub hash: cs::rhash_head,
    pub hash_val: u64,

    pub flags: c_ulong,
    pub written: u16,
    pub nsets: u8,
    pub nr_key_bits: u8,
    pub version_ondisk: u16,

    pub format: cs::bkey_format,

    /*
     * Per-field unpack constants, derived from @format at node init.
     * Extract each field with:
     *
     *   field = (load_8_unaligned(bytes + byte_offset) >> (64 - bits))
     *           + field_offset
     *
     * Load position chosen so the field ends at the top of the loaded
     * value (load_offset + 8 == byte after field's MSB byte); junk from
     * earlier-in-memory fields lands in the low bits and shifts off.
     *
     * byte_offset is signed: for a field near the start of @in, the
     * load can need to start before @in. The byte(s) before @in are
     * always valid memory in the callers we care about (bset payload
     * after the bset header, or other bkeys in the same bset).
     *
     * Only handles formats where every field's MSB sits at a byte
     * boundary (field_msb_bit % 8 == 7). bch2_bkey_format_done()
     * rounds fields up to byte width when there are spare bits, so
     * this is the common case. Formats too tight to byte-align take
     * the slow path via byte_aligned_fields = false.
     */
    pub byte_aligned_fields: bool,
    pub unpack: [cs::bkey_unpack_field; cs::BKEY_NR_FIELDS as usize],

    pub data: *mut Head<cs::btree_node>,
    pub aux_data: *mut c_void,

    /*
     * Sets of sorted keys - the real btree node - plus a binary search tree
     *
     * set[0] is special; set[0]->tree, set[0]->prev and set[0]->data point
     * to the memory we have allocated for this btree node. Additionally,
     * set[0]->data points to the entire btree node as it exists on disk.
     */
    pub set: [cs::bset_tree; cs::MAX_BSETS as usize],

    pub nr: cs::btree_nr_keys,
    pub sib_u64s: [u16; 2],
    pub whiteout_u64s: u16,
    pub byte_order: u8,
    pub unpack_fn_len: u8,

    pub writes: [cs::btree_write; 2],

    /* Key/pointer for this btree node */
    pub key: cs::bkey_i,
    pub key_pad: [u64; cs::BKEY_BTREE_PTR_VAL_U64s_MAX],

    /*
     * XXX: add a delete sequence number, so when bch2_btree_node_relock()
     * fails because the lock sequence number has changed - i.e. the
     * contents were modified - we can still relock the node if it's still
     * the one we want, without redoing the traversal
     */

    /*
     * For asynchronous splits/interior node updates:
     * When we do a split, we allocate new child nodes and update the parent
     * node to point to them: we update the parent in memory immediately,
     * but then we must wait until the children have been written out before
     * the update to the parent can be written - this is a list of the
     * btree_updates that are blocking this node from being
     * written:
     */
    pub write_blocked: cs::list_head,

    /*
     * Also for asynchronous splits/interior node updates:
     * If a btree node isn't reachable yet, we don't want to kick off
     * another write - because that write also won't yet be reachable and
     * marking it as completed before it's reachable would be incorrect:
     */
    pub will_make_reachable: c_ulong,

    pub ob: cs::open_buckets,

    /* lru list */
    pub list: cs::list_head,

    pub cache_state: cs::btree_node_cache_state,
}
c_default!(btree);

c_enum! {
    #[closed]
    pub enum btree_node_sibling: u32 {
        btree_prev_sib,
        btree_next_sib,
    }
}

/* Btree cache: */

c_xmacro! {
    BCH_BTREE_CACHE_NOT_FREED_REASONS(x) {
        (cache_reserve),
        (lock_intent),
        (lock_write),
        (dirty),
        (read_in_flight),
        (write_in_flight),
        (permanent),
        (noevict),
        (write_blocked),
        (will_make_reachable),
        (access_bit),
    }
}

c_enum! {
    #[closed]
    pub enum bch_btree_cache_not_freed_reasons: u32 {
        BCH_BTREE_CACHE_NOT_FREED_REASONS(n) => BCH_BTREE_CACHE_NOT_FREED_ ## n,
        BCH_BTREE_CACHE_NOT_FREED_REASONS_NR,
    }
}

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct btree_cache_list {
    pub idx: c_uint,
    pub shrink: *mut cs::shrinker,
    pub nr_clean: usize,
    pub nr_dirty: usize,
    pub clean: cs::list_head,
    pub dirty: cs::list_head,
}
c_default!(btree_cache_list);

#[repr(C)]
#[derive(CStruct)]
pub struct btree_root {
    pub b: *mut cs::btree,

    /* On disk root - see async splits: */
    pub key: cs::bkey_i,
    pub key_pad: [u64; cs::BKEY_BTREE_PTR_VAL_U64s_MAX],
    pub level: u8,
    pub alive: u8,
    pub error: i16,
}
c_default!(btree_root);

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_fs_btree_cache {
    /*
     * Hot path: btree_path_lock_root reads root pointer + level per
     * btree_id. We pack the level into the low 3 bits of the pointer so a
     * single load yields both atomically (no torn read between b and
     * b->c.level, no extra cacheline miss into the btree node to read
     * level). See bch2_btree_root_{pack,unpack_b,unpack_level} in cache.h.
     *
     * Splitting this out of struct btree_root also keeps the read-side
     * working set in a few cache lines instead of the full ~88 lines of
     * roots_known[].
     */
    pub roots_b: [c_ulong; cs::BTREE_ID_NR as usize],

    pub roots_known: [cs::btree_root; cs::BTREE_ID_NR as usize],
    pub roots_extra: DArray<cs::btree_root>,
    pub root_lock: cs::mutex,

    pub table: cs::rhashtable,
    pub table_init_done: bool,
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
    pub lock: cs::mutex_noio,
    pub freeable: cs::list_head,
    pub freed_pcpu: cs::list_head,
    pub freed_nonpcpu: cs::list_head,
    pub live: [cs::btree_cache_list; 2],

    pub nr_vmalloc: usize,
    pub nr_freeable: usize,
    pub nr_reserve: usize,
    pub nr_by_btree: [usize; cs::BTREE_ID_NR as usize],

    /* Number of nodes with BTREE_NODE_write_in_flight set. */
    pub nr_in_flight: cs::atomic_long_t,
    pub nr_in_flight_inner: cs::atomic_long_t,
    pub nr_in_flight_wait: cs::closure_waitlist,
    pub __should_throttle_align: CacheAligned,
    pub should_throttle: bool,

    /* shrinker stats */
    pub nr_freed: usize,
    pub nr_requested: usize,
    pub not_freed: [u64; cs::BCH_BTREE_CACHE_NOT_FREED_REASONS_NR as usize],

    /*
     * Times the allocator hit the memory-pressure self reclaim path:
     * journal replay watches for this going nonzero to switch off the
     * sorted-order fastpath, which holds every journal pin until replay
     * finishes - pins must be released for reclaim to clean btree nodes.
     */
    pub nr_self_reclaim: c_ulong,

    /*
     * If we need to allocate memory for a new btree node and that
     * allocation fails, we can cannibalize another node in the btree cache
     * to satisfy the allocation - lock to guarantee only one thread does
     * this at a time:
     */
    pub alloc_lock: *mut cs::task_struct,
    pub alloc_wait: cs::closure_waitlist,

    pub pinned_nodes_start: cs::bbpos,
    pub pinned_nodes_end: cs::bbpos,
    /* btree id mask: 0 for leaves, 1 for interior */
    pub pinned_nodes_mask: [u64; 2],
}
c_default!(bch_fs_btree_cache);

/* Iterator, update, and trigger flags: */

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct btree_node_iter_set {
    pub k: u16,
    pub end: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct btree_node_iter {
    pub data: [cs::btree_node_iter_set; cs::MAX_BSETS as usize],
}

c_xmacro! {
    BTREE_ITER_FLAGS(x) {
        (slots),
        (prev),
        (intent),
        (prefetch),
        (is_extents),
        (not_extents),
        (cached),
        (with_key_cache),
        (with_journal),
        (snapshot_field),
        (all_snapshots),
        (filter_snapshots),
        (nofilter_whiteouts),
        (nopreserve),
        (nofill),
        (cached_nofill),
        (key_cache_fill),
        (committed),
        (trim),
    }
}

c_xmacro! {
    BTREE_UPDATE_FLAGS(x) {
        (internal_snapshot_node),
        (nojournal),
        (key_cache_reclaim),
        (overwrite_triggered),
    }
}

c_xmacro! {
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
    BTREE_TRIGGER_FLAGS(x) {
        (norun),
        (transactional),
        (atomic),
        (gc),
        (insert),
        (overwrite),
        (is_discard),
        (set_needs_reconcile_done),
    }
}

c_enum! {
    #[closed]
    pub enum _: u32 {
        BTREE_ITER_FLAGS(n) => BTREE_ITER_FLAG_BIT_ ## n,
        BTREE_UPDATE_FLAGS(n) => BTREE_ITER_FLAG_BIT_ ## n,
        BTREE_TRIGGER_FLAGS(n) => BTREE_ITER_FLAG_BIT_ ## n,
    }
}

c_enum! {
    #[flags]
    pub enum btree_iter_update_trigger_flags: u32 {
        BTREE_ITER_FLAGS(n) => BTREE_ITER_ ## n = 1 << (cs::BTREE_ITER_FLAG_BIT_ ## n as u32),
        BTREE_UPDATE_FLAGS(n) => BTREE_UPDATE_ ## n = 1 << (cs::BTREE_ITER_FLAG_BIT_ ## n as u32),
        BTREE_TRIGGER_FLAGS(n) => BTREE_TRIGGER_ ## n = 1 << (cs::BTREE_ITER_FLAG_BIT_ ## n as u32),
    }
}

#[repr(C)]
#[derive(Default, CStruct)]
pub struct btree_trigger_op {
    pub btree: cs::btree_id,
    pub level: c_uint,
    pub old: cs::bkey_s_c,
    pub new: cs::bkey_s,
    pub new_buf_u64s: c_uint,
    pub flags: cs::btree_iter_update_trigger_flags,
}

// Btree paths and iterators:

c_typedef! {
    pub type btree_path_idx_t = u16;
}

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct btree_path_level {
    pub b: *mut cs::btree,
    pub iter: cs::btree_node_iter,
    pub lock_seq: u32,
    #[cfg(CONFIG_BCACHEFS_LOCK_TIME_STATS)]
    pub lock_taken_time: u64,
}
c_default!(btree_path_level);

nest! {
    #[repr(C)]
    #[derive(Clone, Copy, CStruct)]
    pub struct btree_path {
        pub sorted_idx: cs::btree_path_idx_t,
        pub ref_: u8,
        pub intent_ref: u8,

        /* btree_iter_copy starts here: */
        pub pos: cs::bpos,

        #[c_bitfield]
        #>[bitfield(u16, ty = u32)]
        pub bits: pub struct btree_path_bits {
            #[bits(7)] pub btree_id: cs::btree_id,
            cached:           bool,
            preserve:         bool,
            /*
             * When true, failing to relock this path will cause the transaction to
             * restart:
             */
            should_be_locked: bool,
            level:            u3,
            locks_want:       u3,
        },
        pub nodes_locked: u8,

        pub l: [cs::btree_path_level; cs::BTREE_MAX_DEPTH as usize],
        /* C's TRACK_PATH_ALLOCATED, which btree/types_inline.h defines from
         * these: rustc sees only the configuration */
        #[cfg(any(CONFIG_BCACHEFS_LOCK_TIME_STATS, CONFIG_BCACHEFS_DEBUG))]
        pub ip_allocated: c_ulong,
    }
}
c_default!(btree_path);

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
    #[derive(CStruct)]
    #[c_name(btree_iter)]
    #[cfg_attr(kernel, cfi_encoding = "10btree_iter")]
    pub struct BtreeIter<'t> {
        pub(in crate::btree) trans: *mut cs::btree_trans,
        pub(in crate::btree) path: cs::btree_path_idx_t,
        pub(in crate::btree) update_path: cs::btree_path_idx_t,
        #[c("btree_path_idx_t key_cache_path")]
        pub(in crate::btree) key_cache_path: core::cell::Cell<cs::btree_path_idx_t>,

        #[c_bitfield]
        #>[bitfield(u8)]
        pub(in crate::btree) bits: pub struct btree_iter_bits {
            #[bits(8)] pub(self) btree_id: cs::btree_id,
        },
        pub(in crate::btree) min_depth: u8,

        /* btree_iter_copy starts here: */
        pub(in crate::btree) flags: u32,

        /* When we're filtering by snapshot, the snapshot ID we're looking for: */
        pub(in crate::btree) snapshot: c_uint,

        pub(in crate::btree) pos: cs::bpos,
        /*
         * Current unpacked key - so that bch2_btree_iter_next()/
         * bch2_btree_iter_next_slot() can correctly advance pos.
         */
        pub(in crate::btree) k: cs::bkey,

        /* BTREE_ITER_with_journal: */
        pub(in crate::btree) journal_idx: usize,
        /* C's TRACK_PATH_ALLOCATED: see btree_path */
        #[cfg(any(CONFIG_BCACHEFS_LOCK_TIME_STATS, CONFIG_BCACHEFS_DEBUG))]
        pub(in crate::btree) ip_allocated: c_ulong,

        /* It mustn't outlive the transaction its paths are in */
        #[c_anon("")]
        _trans: core::marker::PhantomData<&'t cs::btree_trans>,
    }
}

impl BtreeIter<'_> {
    /// Set only by C, when it initializes the iterator.
    pub fn btree(&self) -> cs::btree_id {
        self.bits.btree_id()
    }
}

/// C's name, as FFI declarations spell it: *mut c::btree_iter.
pub type btree_iter = BtreeIter<'static>;

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct get_locks_fail {
    pub l: c_uint,
    pub b: *mut cs::btree,
}
c_default!(get_locks_fail);

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

    pub flags: c_ulong,
    pub u64s: u16,
    pub key: cs::bkey_cached_key,

    pub hash: cs::rhash_head,

    pub journal: cs::journal_entry_pin,
    pub seq: u64,

    pub k: *mut cs::bkey_i,
    pub rcu: cs::rcu_head,
}
c_default!(bkey_cached);

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
            #[bits(8)] pub btree_id: cs::btree_id,
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
        pub k_buf_u64s: u8,
        pub path: cs::btree_path_idx_t,
        pub k: *mut cs::bkey_i,
        /* key being overwritten: */
        pub old_k: cs::bkey,
        pub old_v: *const cs::bch_val,
        pub ip_allocated: c_ulong,
    }
}
c_default!(btree_insert_entry);

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
    pub fn_: Option<unsafe extern "C" fn(*mut cs::btree_trans, *mut cs::btree_trans_commit_hook) -> c_int>,
    pub next: *mut cs::btree_trans_commit_hook,
}
c_default!(btree_trans_commit_hook);

c_const! {
    pub const BTREE_TRANS_MEM_MAX: u32 = 1 << 16;

    pub const BTREE_TRANS_MAX_LOCK_HOLD_TIME_NS: c_long = cs::NSEC_PER_MSEC as c_long;
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct btree_trans_paths {
    pub nr_paths: c_ulong,
    pub paths: [cs::btree_path; 0],
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct trans_kmalloc_trace {
    pub ip: c_ulong,
    pub bytes: usize,
}

c_typedef! {
    #[c("DARRAY(struct trans_kmalloc_trace) darray_trans_kmalloc_trace")]
    pub type darray_trans_kmalloc_trace = DArray<cs::trans_kmalloc_trace>;
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct btree_trans_subbuf {
    pub base: u16,
    pub u64s: u16,
    pub size: u16,
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
nest! {
    #[repr(C)]
    #[derive(CStruct)]
    pub struct btree_trans {
        pub c: *mut cs::bch_fs,

        pub paths_allocated: *mut c_ulong,
        pub paths: *mut cs::btree_path,
        pub sorted: *mut cs::btree_path_idx_t,
        pub updates: *mut cs::btree_insert_entry,

        /* bump allocator, invalidated on transaction restart */
        pub mem: *mut c_void,
        pub mem_top: c_uint,
        pub mem_bytes: c_uint,
        pub realloc_bytes_required: c_uint,
        #[cfg(CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE)]
        pub trans_kmalloc_trace: cs::darray_trans_kmalloc_trace,

        pub nr_sorted: cs::btree_path_idx_t,
        pub nr_paths: cs::btree_path_idx_t,
        pub nr_paths_max: cs::btree_path_idx_t,
        pub nr_updates: cs::btree_path_idx_t,
        pub shard_cpu: i16,
        pub fn_idx: u8,
        pub lock_must_abort: u8,
        #[c_bitfield]
        #>[bitfield(u64, repr = NeBytes<6>, from = NeBytes::<6>::from_u64, into = NeBytes::<6>::to_u64)]
        pub bits: pub struct btree_trans_bits {
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
            used_mempool:                   bool,
            in_traverse_all:                bool,
            paths_sorted:                   bool,
            memory_allocation_failure:      bool,
            journal_transaction_names:      bool,
            journal_replay_not_finished:    bool,
            notrace_relock_fail:            bool,
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
            __pad17:                        u15,
            #[bits(16)] pub restarted: cs::bch_errcode,
            __pad:                          u16,
        },
        pub restart_count: u32,
        #[cfg(CONFIG_BCACHEFS_INJECT_TRANSACTION_RESTARTS)]
        pub restart_count_this_trans: u32,
        /*
         * Incremented on every successful (non-empty) commit; for detecting
         * that cached state derived from btree reads may be stale:
         */
        pub commit_count: u32,

        pub last_begin_time: u64,
        pub last_begin_ip: c_ulong,
        pub last_restarted_ip: c_ulong,
        #[cfg(CONFIG_BCACHEFS_DEBUG)]
        pub last_restarted_trace: cs::bch_stacktrace,
        pub last_unlock_ip: c_ulong,
        pub srcu_lock_time: c_ulong,
        pub srcu_idx: c_int,
        pub locking_root_id: cs::btree_id,

        pub locking_hash_val: u64,
        pub locking: *mut cs::btree_bkey_cached_common,
        /*
         * Snapshot of locking->{btree}.hash_val at lock-attempt time, used by
         * bch2_six_check_for_deadlock() to detect that the node identity
         * rotated while we were about to sleep on it. 0 for cached entries.
         */
        pub locking_wait: cs::six_lock_waiter,

        /*
         * btree node writes issued in this trans's context are queued here
         * (singly linked via bi_next) instead of being submitted directly —
         * no block layer work happens while we hold btree node locks.
         * Submitted when the trans unlocks, and before waiting on btree
         * node IO (see bch2_btree_node_wait_on_write()).
         */
        pub queued_write_bios: *mut cs::bio,

        pub fn_: *const c_char,

        /* update path: */
        pub journal_entries: cs::btree_trans_subbuf,
        pub accounting: cs::btree_trans_subbuf,

        pub hooks: *mut cs::btree_trans_commit_hook,
        pub journal_pin: *mut cs::journal_entry_pin,

        pub journal_res: cs::journal_res,
        /*
         * Input for BCH_TRANS_COMMIT_no_journal_res: with no reservation to
         * derive a seq from, the caller names the journal entry the btree node
         * write has to pin.
         */
        pub journal_seq_to_pin: u64,
        pub journal_seq: *mut u64,
        pub disk_res: *mut cs::disk_reservation,
        pub flush: *mut cs::closure,

        pub fs_usage_delta: cs::bch_fs_usage_base,

        pub journal_u64s: c_uint,
        pub extra_journal_u64s: u32,

        /*
         * Space a trigger needs on top of @disk_res, charged at commit.
         * Charge sites may disagree on the count, so keep the max: erring
         * high only over-reserves.
         */
        pub extra_disk_res: u64,
        pub extra_disk_res_replicas: u8,

        pub btree_path_down: cs::bkey_i,
        pub btree_path_down_pad: [u64; cs::BKEY_BTREE_PTR_VAL_U64s_MAX],

        #[cfg(CONFIG_DEBUG_LOCK_ALLOC)]
        pub dep_map: cs::lockdep_map,
        /* Entries before this are zeroed out on every bch2_trans_get() call */

        pub list: cs::list_head,
        pub ref_: cs::closure,
        pub rcu: cs::rcu_head,

        #[c("unsigned long _paths_allocated[BITS_TO_LONGS(BTREE_ITER_INITIAL)]")]
        pub _paths_allocated: [c_ulong; bits_to_longs(cs::BTREE_ITER_INITIAL as usize)],
        pub trans_paths: cs::btree_trans_paths,
        pub _paths: [cs::btree_path; cs::BTREE_ITER_INITIAL as usize],
        #[c("btree_path_idx_t _sorted[BTREE_ITER_INITIAL + 4]")]
        pub _sorted: [cs::btree_path_idx_t; cs::BTREE_ITER_INITIAL as usize + 4],
        pub _updates: [cs::btree_insert_entry; cs::BTREE_ITER_INITIAL as usize],
    }
}
c_default!(btree_trans);

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct btree_trans_buf {
    pub trans: *mut cs::btree_trans,
}
c_default!(btree_trans_buf);

#[repr(C)]
#[derive(CStruct)]
pub struct btree_transaction_stats {
    pub duration: cs::bch2_time_stats,
    pub lock_hold_times: cs::bch2_time_stats,
    pub lock_wait_times: cs::bch2_time_stats,
    pub lock: cs::mutex,
    pub nr_max_paths: c_uint,
    pub max_mem: c_uint,
    #[cfg(CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE)]
    pub trans_kmalloc_trace: cs::darray_trans_kmalloc_trace,
    pub max_paths_text: *mut c_char,
}
c_default!(btree_transaction_stats);

c_const! {
    #[c_int]
    pub const BCH_TRANSACTIONS_NR: u32 = 128;
}

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_fs_btree_trans {
    pub lock: cs::seqmutex,
    pub list: cs::list_head,
    pub pool: cs::mempool_t,
    pub malloc_pool: cs::mempool_t,
    #[c("struct btree_trans_buf __percpu *bufs")]
    pub bufs: *mut cs::btree_trans_buf,

    pub barrier: cs::srcu_struct,
    pub barrier_initialized: bool,

    pub stats: [cs::btree_transaction_stats; cs::BCH_TRANSACTIONS_NR as usize],

    pub stats_json_lock: cs::mutex,
    pub stats_json_buf: cs::printbuf,
}
c_default!(bch_fs_btree_trans);

/* Btree node write types and flags: */

c_xmacro! {
    BCH_BTREE_WRITE_TYPES(x) {
        (initial, 0),
        (init_next_bset, 1),
        (cache_reclaim, 2),
        (journal_reclaim, 3),
        (interior, 4),
    }
}

c_enum! {
    #[closed]
    pub enum btree_write_type: u32 {
        BCH_BTREE_WRITE_TYPES(t, ..) => BTREE_WRITE_ ## t,
        BTREE_WRITE_TYPE_NR,
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
        BTREE_FLAGS(flag) => BTREE_NODE_ ## flag,
    }
}

c_xmacro! {
    BTREE_NODE_REWRITE_REASON(x) {
        (none),
        (unknown),
        (error),
        (ptr_written_zero),
    }
}

c_enum! {
    #[closed]
    pub enum btree_node_rewrite_reason: u32 {
        BTREE_NODE_REWRITE_REASON(n) => BTREE_NODE_REWRITE_ ## n,
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
#[derive(Clone, Copy, CStruct)]
pub struct btree_write_stats {
    pub nr: cs::atomic64_t,
    pub bytes: cs::atomic64_t,
}
c_default!(btree_write_stats);

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_fs_btree {
    pub foreground_merge_threshold: u16,

    pub bounce_pool: cs::mempool_t,

    pub write_stats: [cs::btree_write_stats; cs::BTREE_WRITE_TYPE_NR as usize],

    pub bio: cs::bio_set,
    pub fill_iter: cs::mempool_t,
    pub read_complete_wq: *mut cs::workqueue_struct,
    pub read_errors_soft: cs::ratelimit_state,
    pub read_errors_hard: cs::ratelimit_state,

    pub write_complete_wq: *mut cs::workqueue_struct,

    pub root_journal_res: cs::journal_entry_res,

    pub cache: cs::bch_fs_btree_cache,
    pub evicted_size: cs::btree_evicted_size,
    pub key_cache: cs::bch_fs_btree_key_cache,
    /*
     * One per BTREE_IS_write_buffer btree (indexed by BCH_WB_BTREE_*, see
     * bch_wb_btree_idx()). Each instance has its own intake/flushing
     * buffers and locks.
     */
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
    pub write_buffer_wq: *mut cs::workqueue_struct,
    pub write_buffer_shard_wq: *mut cs::workqueue_struct,
    /*
     * Sync flushers (btree_write_buffer_flush_seq) wake the per-btree
     * flush_works and then wait here for the worker to drain pins past
     * their target seq. Waked from the flush worker after drain and from
     * journal_keys_to_write_buffer_end after pin drops.
     */
    pub write_buffer_flush_wait: cs::closure_waitlist,
    pub trans: cs::bch_fs_btree_trans,
    pub reserve_cache: cs::bch_fs_btree_reserve_cache,
    pub interior_updates: cs::bch_fs_btree_interior_updates,
    pub node_rewrites: cs::bch_fs_btree_node_rewrites,
    pub node_scan: cs::find_btree_nodes,
}
c_default!(bch_fs_btree);

/* Btree ID properties: */
c_enum! {
    #[closed]
    pub enum btree_node_type: u32 {
        BKEY_TYPE_btree,
        BCH_BTREE_IDS(kwd, val, ..) => BKEY_TYPE_ ## kwd = (val as u32) + 1,
        BKEY_TYPE_NR,
    }
}

impl c::btree_id {
    /// Convert from raw u32. Returns None for unknown built-in btree IDs.
    pub fn from_raw(id: u32) -> Option<Self> {
        crate::BTREE_IDS_KNOWN.get(id as usize).copied()
    }

    /// Iterate over all known btree IDs.
    pub fn iter_known() -> impl Iterator<Item = Self> {
        crate::BTREE_IDS_KNOWN.iter().copied()
    }
}

impl From<c::btree_id> for u32 {
    fn from(id: c::btree_id) -> u32 {
        id.0
    }
}

/// Get a btree ID name string.
#[cfg(feature = "std")]
pub fn btree_id_str(id: u32) -> String {
    match c::btree_id::from_raw(id) {
        Some(btree_id) => format!("{}", btree_id),
        None => format!("(unknown btree {})", id),
    }
}

impl fmt::Display for c::btree_id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = unsafe { CStr::from_ptr(c::bch2_btree_id_str(*self).cast()) };
        f.write_str(s.to_str().unwrap_or("(invalid btree name)"))
    }
}

#[cfg(feature = "std")]
impl FromStr for c::btree_id {
    type Err = BchError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = CString::new(s)
            .map_err(|_| BchError::from(bch_errcode::BCH_ERR_EINVAL_parse_btree_id))?;
        let p = s.as_ptr();

        let v =
            unsafe { c::match_string(c::__bch2_btree_ids[..].as_ptr(), (-1_isize) as usize, p) };
        c::btree_id::from_raw(v as u32)
            .ok_or(bch_errcode::BCH_ERR_EINVAL_parse_btree_id.into())
    }
}

#[cfg(feature = "std")]
impl FromStr for c::bch_bkey_type {
    type Err = BchError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = CString::new(s)
            .map_err(|_| BchError::from(bch_errcode::BCH_ERR_EINVAL_parse_bkey_type))?;
        let p = s.as_ptr();

        let v = unsafe { c::match_string(c::bch2_bkey_types[..].as_ptr(), (-1_isize) as usize, p) };
        if v >= 0 {
            Ok(c::bch_bkey_type(v as u32))
        } else {
            Err(bch_errcode::BCH_ERR_EINVAL_parse_bkey_type.into())
        }
    }
}
