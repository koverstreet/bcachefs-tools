#![allow(non_camel_case_types)]

use crate::c;
#[cfg(feature = "std")]
use crate::errcode::{bch_errcode, BchError};
use core::{ffi::CStr, fmt};
#[cfg(feature = "std")]
use std::{ffi::CString, str::FromStr};

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
        id.0 as u32
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
// ---- the data types of btree/types.h, which is generated from this file: see
// fs/types/lib.rs. Translated from the C by c2rs.

use crate::cstructs::c as cs;
use crate::cstructs::c::BCH_BTREE_IDS;
use crate::types::{DArray, bits_to_longs, c_default};
use core::ffi::{c_long, c_ulong};
use cstruct_macros::{bitfield, c_const, c_enum, c_typedef, c_verbatim, c_xmacro, CStruct};
use typeinfo_macros::TypeInfo;
c_verbatim!(r#"
struct bio;
struct open_bucket;
struct btree_update;
struct btree_trans;
struct lock_graph;
"#);

c_const! {
    /* Btree nodes: */
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

    pub flags: core::ffi::c_ulong,
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

    pub data: *mut cs::btree_node,
    pub aux_data: *mut core::ffi::c_void,

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
    pub will_make_reachable: core::ffi::c_ulong,

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

c_xmacro! {
    /* Btree cache: */
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

macro_rules! __bch_btree_cache_not_freed_reasons_0 {
    ([$($acc:tt)*] $(($n:tt)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[closed]
            pub enum bch_btree_cache_not_freed_reasons: u32 {
                $($acc)*
                $([<BCH_BTREE_CACHE_NOT_FREED_ $n>],)*
                BCH_BTREE_CACHE_NOT_FREED_REASONS_NR,
            }
        }
    } };
}
BCH_BTREE_CACHE_NOT_FREED_REASONS!(__bch_btree_cache_not_freed_reasons_0 []);

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct btree_cache_list {
    pub idx: core::ffi::c_uint,
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
    pub roots_b: [core::ffi::c_ulong; cs::BTREE_ID_NR as usize],

    pub roots_known: [cs::btree_root; cs::BTREE_ID_NR as usize],
    #[c("DARRAY(struct btree_root) roots_extra")]
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
    #[c_anon("")] pub __should_throttle_align: [crate::types::CacheAligned; 0],
    #[c("bool should_throttle ____cacheline_aligned_in_smp")]
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
    pub nr_self_reclaim: core::ffi::c_ulong,

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

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct btree_node_iter_set {
    pub k: u16,
    pub end: u16,
}

/* Iterator, update, and trigger flags: */
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
    }
}

c_xmacro! {
    STR_HASH_FLAGS(x) {
        (must_create),
        (must_replace),
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

macro_rules! __anon_btree_iter_flags_0 {
    ([$($acc:tt)*] $(($n:tt)),* $(,)?) => { ::paste::paste! {
        STR_HASH_FLAGS!(__anon_btree_iter_flags_1 [$($acc)* $([<BTREE_ITER_FLAG_BIT_ $n>],)*]);
    } };
}
macro_rules! __anon_btree_iter_flags_1 {
    ([$($acc:tt)*] $(($n:tt)),* $(,)?) => { ::paste::paste! {
        BTREE_UPDATE_FLAGS!(__anon_btree_iter_flags_2 [$($acc)* $([<BTREE_ITER_FLAG_BIT_ $n>],)*]);
    } };
}
macro_rules! __anon_btree_iter_flags_2 {
    ([$($acc:tt)*] $(($n:tt)),* $(,)?) => { ::paste::paste! {
        BTREE_TRIGGER_FLAGS!(__anon_btree_iter_flags_3 [$($acc)* $([<BTREE_ITER_FLAG_BIT_ $n>],)*]);
    } };
}
macro_rules! __anon_btree_iter_flags_3 {
    ([$($acc:tt)*] $(($n:tt)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[closed]
            pub enum _: u32 {
                $($acc)*
                $([<BTREE_ITER_FLAG_BIT_ $n>],)*
            }
        }
    } };
}
BTREE_ITER_FLAGS!(__anon_btree_iter_flags_0 []);

macro_rules! __btree_iter_update_trigger_flags_0 {
    ([$($acc:tt)*] $(($n:tt)),* $(,)?) => { ::paste::paste! {
        STR_HASH_FLAGS!(__btree_iter_update_trigger_flags_1 [$($acc)* $([<BTREE_ITER_ $n>] = 1 << (cs::[<BTREE_ITER_FLAG_BIT_ $n>] as u32),)*]);
    } };
}
macro_rules! __btree_iter_update_trigger_flags_1 {
    ([$($acc:tt)*] $(($n:tt)),* $(,)?) => { ::paste::paste! {
        BTREE_UPDATE_FLAGS!(__btree_iter_update_trigger_flags_2 [$($acc)* $([<STR_HASH_ $n>] = 1 << (cs::[<BTREE_ITER_FLAG_BIT_ $n>] as u32),)*]);
    } };
}
macro_rules! __btree_iter_update_trigger_flags_2 {
    ([$($acc:tt)*] $(($n:tt)),* $(,)?) => { ::paste::paste! {
        BTREE_TRIGGER_FLAGS!(__btree_iter_update_trigger_flags_3 [$($acc)* $([<BTREE_UPDATE_ $n>] = 1 << (cs::[<BTREE_ITER_FLAG_BIT_ $n>] as u32),)*]);
    } };
}
macro_rules! __btree_iter_update_trigger_flags_3 {
    ([$($acc:tt)*] $(($n:tt)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[flags]
            pub enum btree_iter_update_trigger_flags: u32 {
                $($acc)*
                $([<BTREE_TRIGGER_ $n>] = 1 << (cs::[<BTREE_ITER_FLAG_BIT_ $n>] as u32),)*
            }
        }
    } };
}
BTREE_ITER_FLAGS!(__btree_iter_update_trigger_flags_0 []);

#[repr(C)]
#[derive(CStruct)]
pub struct btree_trigger_op {
    pub btree: cs::btree_id,
    pub level: core::ffi::c_uint,
    pub old: cs::bkey_s_c,
    pub new: cs::bkey_s,
    pub new_buf_u64s: core::ffi::c_uint,
    pub flags: cs::btree_iter_update_trigger_flags,
}
c_default!(btree_trigger_op);

c_verbatim!(r#"
/* Btree paths and iterators: */
"#);

#[cfg(any(CONFIG_BCACHEFS_LOCK_TIME_STATS, CONFIG_BCACHEFS_DEBUG))]
c_verbatim!(r#"
#define TRACK_PATH_ALLOCATED
"#);

c_typedef! {
    pub type btree_path_idx_t = u16;
}

#[bitfield(u16, repr = zerocopy::byteorder::native_endian::U16, from = zerocopy::byteorder::native_endian::U16::new, into = zerocopy::byteorder::native_endian::U16::get)]
pub struct btree_path_btree_id_bits {
    #[bits(7)]
    pub btree_id: u32,
    #[bits(1)]
    pub cached: bool,
    #[bits(1)]
    pub preserve: bool,
    /*
     * When true, failing to relock this path will cause the transaction to
     * restart:
     */
    #[bits(1)]
    pub should_be_locked: bool,
    #[bits(3)]
    pub level: u32,
    #[bits(3)]
    pub locks_want: u32,
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

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct btree_path {
    pub sorted_idx: cs::btree_path_idx_t,
    pub ref_: u8,
    pub intent_ref: u8,

    /* btree_iter_copy starts here: */
    pub pos: cs::bpos,

    #[c_bitfield]
    pub btree_id_bits: btree_path_btree_id_bits,
    pub nodes_locked: u8,

    pub l: [cs::btree_path_level; cs::BTREE_MAX_DEPTH as usize],
    /* C's TRACK_PATH_ALLOCATED, which it defines from these, above:
     * rustc sees only the configuration */
    #[cfg(any(CONFIG_BCACHEFS_LOCK_TIME_STATS, CONFIG_BCACHEFS_DEBUG))]
    pub ip_allocated: core::ffi::c_ulong,
}
c_default!(btree_path);
impl btree_path {
    pub fn btree_id(&self) -> u32 { let b = self.btree_id_bits; b.btree_id() }
    pub fn set_btree_id(&mut self, v: u32) { let mut b = self.btree_id_bits; b.set_btree_id(v); self.btree_id_bits = b; }
    pub fn cached(&self) -> bool { let b = self.btree_id_bits; b.cached() }
    pub fn set_cached(&mut self, v: bool) { let mut b = self.btree_id_bits; b.set_cached(v); self.btree_id_bits = b; }
    pub fn preserve(&self) -> bool { let b = self.btree_id_bits; b.preserve() }
    pub fn set_preserve(&mut self, v: bool) { let mut b = self.btree_id_bits; b.set_preserve(v); self.btree_id_bits = b; }
    pub fn should_be_locked(&self) -> bool { let b = self.btree_id_bits; b.should_be_locked() }
    pub fn set_should_be_locked(&mut self, v: bool) { let mut b = self.btree_id_bits; b.set_should_be_locked(v); self.btree_id_bits = b; }
    pub fn level(&self) -> u32 { let b = self.btree_id_bits; b.level() }
    pub fn set_level(&mut self, v: u32) { let mut b = self.btree_id_bits; b.set_level(v); self.btree_id_bits = b; }
    pub fn locks_want(&self) -> u32 { let b = self.btree_id_bits; b.locks_want() }
    pub fn set_locks_want(&mut self, v: u32) { let mut b = self.btree_id_bits; b.set_locks_want(v); self.btree_id_bits = b; }
}

#[bitfield(u8)]
pub struct btree_iter_btree_id_bits {
    #[bits(8)]
    pub btree_id: u32,
}

/*
 * btree_iter: the high level btree iterator API, iterates over keys.
 * btree_path: the low level path to a btree node, holds locks.
 *
 * Multiple iterators can share the same btree_path via refcounting.
 */
#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct btree_iter {
    pub trans: *mut cs::btree_trans,
    pub path: cs::btree_path_idx_t,
    pub update_path: cs::btree_path_idx_t,
    pub key_cache_path: cs::btree_path_idx_t,

    #[c_bitfield]
    pub btree_id_bits: btree_iter_btree_id_bits,
    pub min_depth: u8,

    /* btree_iter_copy starts here: */
    pub flags: u32,

    /* When we're filtering by snapshot, the snapshot ID we're looking for: */
    pub snapshot: core::ffi::c_uint,

    pub pos: cs::bpos,
    /*
     * Current unpacked key - so that bch2_btree_iter_next()/
     * bch2_btree_iter_next_slot() can correctly advance pos.
     */
    pub k: cs::bkey,

    /* BTREE_ITER_with_journal: */
    pub journal_idx: usize,
    /* C's TRACK_PATH_ALLOCATED: see btree_path */
    #[cfg(any(CONFIG_BCACHEFS_LOCK_TIME_STATS, CONFIG_BCACHEFS_DEBUG))]
    pub ip_allocated: core::ffi::c_ulong,
}
c_default!(btree_iter);
impl btree_iter {
    pub fn btree_id(&self) -> u32 { let b = self.btree_id_bits; b.btree_id() }
    pub fn set_btree_id(&mut self, v: u32) { let mut b = self.btree_id_bits; b.set_btree_id(v); self.btree_id_bits = b; }
}

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct get_locks_fail {
    pub l: core::ffi::c_uint,
    pub b: *mut cs::btree,
}
c_default!(get_locks_fail);

c_const! {
    /* Key cache: */
    #[c_int]
    pub const BKEY_CACHED_ACCESSED: u32 = 0;
}

c_const! {
    #[c_int]
    pub const BKEY_CACHED_DIRTY: u32 = 1;
}

c_const! {
    #[c_int]
    pub const BKEY_CACHED_IMMEDIATE_FLUSH: u32 = 2;
}

#[repr(C)]
#[derive(CStruct)]
pub struct bkey_cached {
    pub c: cs::btree_bkey_cached_common,

    pub flags: core::ffi::c_ulong,
    pub u64s: u16,
    pub key: cs::bkey_cached_key,

    pub hash: cs::rhash_head,

    pub journal: cs::journal_entry_pin,
    pub seq: u64,

    pub k: *mut cs::bkey_i,
    pub rcu: cs::rcu_head,
}
c_default!(bkey_cached);

#[bitfield(u16, repr = zerocopy::byteorder::native_endian::U16, from = zerocopy::byteorder::native_endian::U16::new, into = zerocopy::byteorder::native_endian::U16::get)]
pub struct btree_insert_entry_btree_id_bits {
    #[bits(8)]
    pub btree_id: u32,
    #[bits(3)]
    pub level: u8,
    #[bits(1)]
    pub cached: bool,
    #[bits(1)]
    pub insert_trigger_run: bool,
    #[bits(1)]
    pub overwrite_trigger_run: bool,
    #[bits(1)]
    pub key_cache_already_flushed: bool,
    #[bits(1)]
    pub key_cache_flushing: bool,
}

/* Transaction types: */
#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct btree_insert_entry {
    pub flags: core::ffi::c_uint,
    pub sort_order: u8,
    pub bkey_type: u8,
    #[c_bitfield]
    pub btree_id_bits: btree_insert_entry_btree_id_bits,
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
    pub ip_allocated: core::ffi::c_ulong,
}
c_default!(btree_insert_entry);
impl btree_insert_entry {
    pub fn btree_id(&self) -> u32 { let b = self.btree_id_bits; b.btree_id() }
    pub fn set_btree_id(&mut self, v: u32) { let mut b = self.btree_id_bits; b.set_btree_id(v); self.btree_id_bits = b; }
    pub fn level(&self) -> u8 { let b = self.btree_id_bits; b.level() }
    pub fn set_level(&mut self, v: u8) { let mut b = self.btree_id_bits; b.set_level(v); self.btree_id_bits = b; }
    pub fn cached(&self) -> bool { let b = self.btree_id_bits; b.cached() }
    pub fn set_cached(&mut self, v: bool) { let mut b = self.btree_id_bits; b.set_cached(v); self.btree_id_bits = b; }
    pub fn insert_trigger_run(&self) -> bool { let b = self.btree_id_bits; b.insert_trigger_run() }
    pub fn set_insert_trigger_run(&mut self, v: bool) { let mut b = self.btree_id_bits; b.set_insert_trigger_run(v); self.btree_id_bits = b; }
    pub fn overwrite_trigger_run(&self) -> bool { let b = self.btree_id_bits; b.overwrite_trigger_run() }
    pub fn set_overwrite_trigger_run(&mut self, v: bool) { let mut b = self.btree_id_bits; b.set_overwrite_trigger_run(v); self.btree_id_bits = b; }
    pub fn key_cache_already_flushed(&self) -> bool { let b = self.btree_id_bits; b.key_cache_already_flushed() }
    pub fn set_key_cache_already_flushed(&mut self, v: bool) { let mut b = self.btree_id_bits; b.set_key_cache_already_flushed(v); self.btree_id_bits = b; }
    pub fn key_cache_flushing(&self) -> bool { let b = self.btree_id_bits; b.key_cache_flushing() }
    pub fn set_key_cache_flushing(&mut self, v: bool) { let mut b = self.btree_id_bits; b.set_key_cache_flushing(v); self.btree_id_bits = b; }
}

c_const! {
    /* Number of btree paths we preallocate, usually enough */
    #[c_int]
    pub const BTREE_ITER_INITIAL: u32 = 64;
}

c_const! {
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
}

c_const! {
    /* never exceed limit */
    pub const BTREE_ITER_MAX: u32 = 1 << 10;
}

c_verbatim!(r#"
struct btree_trans_commit_hook;
typedef int (btree_trans_commit_hook_fn)(struct btree_trans *, struct btree_trans_commit_hook *);
"#);

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct btree_trans_commit_hook {
    #[c("btree_trans_commit_hook_fn *fn")]
    pub fn_: Option<unsafe extern "C" fn(*mut cs::btree_trans, *mut cs::btree_trans_commit_hook) -> core::ffi::c_int>,
    pub next: *mut cs::btree_trans_commit_hook,
}
c_default!(btree_trans_commit_hook);

c_const! {
    pub const BTREE_TRANS_MEM_MAX: u32 = 1 << 16;
}

c_const! {
    pub const BTREE_TRANS_MAX_LOCK_HOLD_TIME_NS: c_long = cs::NSEC_PER_MSEC as c_long;
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct btree_trans_paths {
    pub nr_paths: core::ffi::c_ulong,
    pub paths: [cs::btree_path; 0],
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct trans_kmalloc_trace {
    pub ip: core::ffi::c_ulong,
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

#[bitfield(u64, repr = crate::types::NeBytes::<6>, from = crate::types::NeBytes::<6>::from_u64, into = crate::types::NeBytes::<6>::to_u64)]
pub struct btree_trans_lock_may_not_fail_bits {
    #[bits(1)]
    pub lock_may_not_fail: bool,
    #[bits(1)]
    pub locked: bool,
    #[bits(1)]
    pub migrate_disabled: bool,
    #[bits(1)]
    pub write_locked: bool,
    #[bits(1)]
    pub srcu_held: bool,
    /*
     * IO was submitted under the srcu read lock (submit_bio() can block for
     * an unbounded time): suppress the next bch2_trans_unlock_long() "held
     * too long" warning. Scoped to one locked attempt — cleared on relock /
     * bch2_trans_begin().
     */
    #[bits(1)]
    pub srcu_io_submitted: bool,
    #[bits(1)]
    pub btree_cache_cannibalize_locked: bool,
    #[bits(1)]
    pub pf_memalloc_noio: bool,
    #[bits(1)]
    pub used_mempool: bool,
    #[bits(1)]
    pub in_traverse_all: bool,
    #[bits(1)]
    pub paths_sorted: bool,
    #[bits(1)]
    pub memory_allocation_failure: bool,
    #[bits(1)]
    pub journal_transaction_names: bool,
    #[bits(1)]
    pub journal_replay_not_finished: bool,
    #[bits(1)]
    pub notrace_relock_fail: bool,
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
    #[bits(1)]
    pub begin_may_drop_updates: bool,
    #[bits(1)]
    pub has_interior_updates: bool,
    #[bits(15)]
    pub __pad17: u16,
    #[bits(16)]
    pub restarted: u32,
    #[bits(16)]
    pub __pad: u16,
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
#[derive(CStruct)]
pub struct btree_trans {
    pub c: *mut cs::bch_fs,

    pub paths_allocated: *mut core::ffi::c_ulong,
    pub paths: *mut cs::btree_path,
    pub sorted: *mut cs::btree_path_idx_t,
    pub updates: *mut cs::btree_insert_entry,

    /* bump allocator, invalidated on transaction restart */
    pub mem: *mut core::ffi::c_void,
    pub mem_top: core::ffi::c_uint,
    pub mem_bytes: core::ffi::c_uint,
    pub realloc_bytes_required: core::ffi::c_uint,
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
    pub lock_may_not_fail_bits: btree_trans_lock_may_not_fail_bits,
    pub restart_count: u32,
    #[cfg(CONFIG_BCACHEFS_INJECT_TRANSACTION_RESTARTS)]
    pub restart_count_this_trans: u32,
    /*
     * Incremented on every successful (non-empty) commit; for detecting
     * that cached state derived from btree reads may be stale:
     */
    pub commit_count: u32,

    pub last_begin_time: u64,
    pub last_begin_ip: core::ffi::c_ulong,
    pub last_restarted_ip: core::ffi::c_ulong,
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    pub last_restarted_trace: cs::bch_stacktrace,
    pub last_unlock_ip: core::ffi::c_ulong,
    pub srcu_lock_time: core::ffi::c_ulong,
    pub srcu_idx: core::ffi::c_int,
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

    pub fn_: *const crate::util::ffi::c_char,

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

    pub journal_u64s: core::ffi::c_uint,
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
    pub _paths_allocated: [core::ffi::c_ulong; bits_to_longs(cs::BTREE_ITER_INITIAL as usize)],
    pub trans_paths: cs::btree_trans_paths,
    pub _paths: [cs::btree_path; cs::BTREE_ITER_INITIAL as usize],
    #[c("btree_path_idx_t _sorted[BTREE_ITER_INITIAL + 4]")]
    pub _sorted: [cs::btree_path_idx_t; cs::BTREE_ITER_INITIAL as usize + 4],
    pub _updates: [cs::btree_insert_entry; cs::BTREE_ITER_INITIAL as usize],
}
c_default!(btree_trans);
impl btree_trans {
    pub fn lock_may_not_fail(&self) -> bool { let b = self.lock_may_not_fail_bits; b.lock_may_not_fail() }
    pub fn set_lock_may_not_fail(&mut self, v: bool) { let mut b = self.lock_may_not_fail_bits; b.set_lock_may_not_fail(v); self.lock_may_not_fail_bits = b; }
    pub fn locked(&self) -> bool { let b = self.lock_may_not_fail_bits; b.locked() }
    pub fn set_locked(&mut self, v: bool) { let mut b = self.lock_may_not_fail_bits; b.set_locked(v); self.lock_may_not_fail_bits = b; }
    pub fn migrate_disabled(&self) -> bool { let b = self.lock_may_not_fail_bits; b.migrate_disabled() }
    pub fn set_migrate_disabled(&mut self, v: bool) { let mut b = self.lock_may_not_fail_bits; b.set_migrate_disabled(v); self.lock_may_not_fail_bits = b; }
    pub fn write_locked(&self) -> bool { let b = self.lock_may_not_fail_bits; b.write_locked() }
    pub fn set_write_locked(&mut self, v: bool) { let mut b = self.lock_may_not_fail_bits; b.set_write_locked(v); self.lock_may_not_fail_bits = b; }
    pub fn srcu_held(&self) -> bool { let b = self.lock_may_not_fail_bits; b.srcu_held() }
    pub fn set_srcu_held(&mut self, v: bool) { let mut b = self.lock_may_not_fail_bits; b.set_srcu_held(v); self.lock_may_not_fail_bits = b; }
    pub fn srcu_io_submitted(&self) -> bool { let b = self.lock_may_not_fail_bits; b.srcu_io_submitted() }
    pub fn set_srcu_io_submitted(&mut self, v: bool) { let mut b = self.lock_may_not_fail_bits; b.set_srcu_io_submitted(v); self.lock_may_not_fail_bits = b; }
    pub fn btree_cache_cannibalize_locked(&self) -> bool { let b = self.lock_may_not_fail_bits; b.btree_cache_cannibalize_locked() }
    pub fn set_btree_cache_cannibalize_locked(&mut self, v: bool) { let mut b = self.lock_may_not_fail_bits; b.set_btree_cache_cannibalize_locked(v); self.lock_may_not_fail_bits = b; }
    pub fn pf_memalloc_noio(&self) -> bool { let b = self.lock_may_not_fail_bits; b.pf_memalloc_noio() }
    pub fn set_pf_memalloc_noio(&mut self, v: bool) { let mut b = self.lock_may_not_fail_bits; b.set_pf_memalloc_noio(v); self.lock_may_not_fail_bits = b; }
    pub fn used_mempool(&self) -> bool { let b = self.lock_may_not_fail_bits; b.used_mempool() }
    pub fn set_used_mempool(&mut self, v: bool) { let mut b = self.lock_may_not_fail_bits; b.set_used_mempool(v); self.lock_may_not_fail_bits = b; }
    pub fn in_traverse_all(&self) -> bool { let b = self.lock_may_not_fail_bits; b.in_traverse_all() }
    pub fn set_in_traverse_all(&mut self, v: bool) { let mut b = self.lock_may_not_fail_bits; b.set_in_traverse_all(v); self.lock_may_not_fail_bits = b; }
    pub fn paths_sorted(&self) -> bool { let b = self.lock_may_not_fail_bits; b.paths_sorted() }
    pub fn set_paths_sorted(&mut self, v: bool) { let mut b = self.lock_may_not_fail_bits; b.set_paths_sorted(v); self.lock_may_not_fail_bits = b; }
    pub fn memory_allocation_failure(&self) -> bool { let b = self.lock_may_not_fail_bits; b.memory_allocation_failure() }
    pub fn set_memory_allocation_failure(&mut self, v: bool) { let mut b = self.lock_may_not_fail_bits; b.set_memory_allocation_failure(v); self.lock_may_not_fail_bits = b; }
    pub fn journal_transaction_names(&self) -> bool { let b = self.lock_may_not_fail_bits; b.journal_transaction_names() }
    pub fn set_journal_transaction_names(&mut self, v: bool) { let mut b = self.lock_may_not_fail_bits; b.set_journal_transaction_names(v); self.lock_may_not_fail_bits = b; }
    pub fn journal_replay_not_finished(&self) -> bool { let b = self.lock_may_not_fail_bits; b.journal_replay_not_finished() }
    pub fn set_journal_replay_not_finished(&mut self, v: bool) { let mut b = self.lock_may_not_fail_bits; b.set_journal_replay_not_finished(v); self.lock_may_not_fail_bits = b; }
    pub fn notrace_relock_fail(&self) -> bool { let b = self.lock_may_not_fail_bits; b.notrace_relock_fail() }
    pub fn set_notrace_relock_fail(&mut self, v: bool) { let mut b = self.lock_may_not_fail_bits; b.set_notrace_relock_fail(v); self.lock_may_not_fail_bits = b; }
    pub fn begin_may_drop_updates(&self) -> bool { let b = self.lock_may_not_fail_bits; b.begin_may_drop_updates() }
    pub fn set_begin_may_drop_updates(&mut self, v: bool) { let mut b = self.lock_may_not_fail_bits; b.set_begin_may_drop_updates(v); self.lock_may_not_fail_bits = b; }
    pub fn has_interior_updates(&self) -> bool { let b = self.lock_may_not_fail_bits; b.has_interior_updates() }
    pub fn set_has_interior_updates(&mut self, v: bool) { let mut b = self.lock_may_not_fail_bits; b.set_has_interior_updates(v); self.lock_may_not_fail_bits = b; }
    pub fn restarted(&self) -> u32 { let b = self.lock_may_not_fail_bits; b.restarted() }
    pub fn set_restarted(&mut self, v: u32) { let mut b = self.lock_may_not_fail_bits; b.set_restarted(v); self.lock_may_not_fail_bits = b; }
}

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
    pub nr_max_paths: core::ffi::c_uint,
    pub max_mem: core::ffi::c_uint,
    #[cfg(CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE)]
    pub trans_kmalloc_trace: cs::darray_trans_kmalloc_trace,
    pub max_paths_text: *mut crate::util::ffi::c_char,
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

c_xmacro! {
    /* Btree node write types and flags: */
    BCH_BTREE_WRITE_TYPES(x) {
        (initial, 0),
        (init_next_bset, 1),
        (cache_reclaim, 2),
        (journal_reclaim, 3),
        (interior, 4),
    }
}

macro_rules! __btree_write_type_0 {
    ([$($acc:tt)*] $(($t:tt$(, $($__rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[closed]
            pub enum btree_write_type: u32 {
                $($acc)*
                $([<BTREE_WRITE_ $t>],)*
                BTREE_WRITE_TYPE_NR,
            }
        }
    } };
}
BCH_BTREE_WRITE_TYPES!(__btree_write_type_0 []);

c_const! {
    pub const BTREE_WRITE_TYPE_MASK: c_ulong =
        (cs::BTREE_WRITE_TYPE_NR as c_ulong).next_power_of_two() - 1;
}

c_const! {
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

macro_rules! __btree_flags_0 {
    ([$($acc:tt)*] $(($flag:tt)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[closed]
            pub enum btree_flags: u32 {
                $($acc)*
                $([<BTREE_NODE_ $flag>],)*
            }
        }
    } };
}
BTREE_FLAGS!(__btree_flags_0 [
    /* First bits for btree node write type */
    BTREE_NODE_FLAGS_START = cs::BTREE_WRITE_TYPE_BITS - 1,
]);

c_xmacro! {
    BTREE_NODE_REWRITE_REASON(x) {
        (none),
        (unknown),
        (error),
        (ptr_written_zero),
    }
}

macro_rules! __btree_node_rewrite_reason_0 {
    ([$($acc:tt)*] $(($n:tt)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[closed]
            pub enum btree_node_rewrite_reason: u32 {
                $($acc)*
                $([<BTREE_NODE_REWRITE_ $n>],)*
            }
        }
    } };
}
BTREE_NODE_REWRITE_REASON!(__btree_node_rewrite_reason_0 []);

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
}

c_const! {
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

c_verbatim!(r#"
#define btree_bkey_first(_b, _t)					\
({									\
	EBUG_ON(bset(_b, _t)->start !=					\
		__btree_node_offset_to_key(_b, btree_bkey_first_offset(_t)));\
									\
	bset(_b, _t)->start;						\
})

#define btree_bkey_last(_b, _t)						\
({									\
	EBUG_ON(__btree_node_offset_to_key(_b, (_t)->end_offset) !=	\
		vstruct_last(bset(_b, _t)));				\
									\
	__btree_node_offset_to_key(_b, (_t)->end_offset);		\
})
"#);

/* Btree ID properties: */
macro_rules! __btree_node_type_0 {
    ([$($acc:tt)*] $(($kwd:tt, $val:expr$(, $($__rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[closed]
            pub enum btree_node_type: u32 {
                $($acc)*
                $([<BKEY_TYPE_ $kwd>] = (($val) as u32) + 1,)*
                BKEY_TYPE_NR,
            }
        }
    } };
}
BCH_BTREE_IDS!(__btree_node_type_0 [BKEY_TYPE_btree,]);
