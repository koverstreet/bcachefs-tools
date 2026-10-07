// SPDX-License-Identifier: GPL-2.0

//! The data types of btree/interior_defs.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{DArray, c_default};
use zerocopy::byteorder::little_endian as le;
use cstruct_macros::{bitfield, c_const, c_enum, c_typedef, c_xmacro, CStruct};

c_const! {
    pub const BTREE_UPDATE_NODES_MAX: u32 = (c::BTREE_MAX_DEPTH as u32 - 2) * 2 + c::GC_MERGE_NODES;
}

c_xmacro! {
    BTREE_UPDATE_MODES(x) {
        (none),
        (node),
        (root),
        (update),
    }
}

macro_rules! __btree_update_mode_0 {
    ([$($acc:tt)*] $(($n:tt)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[closed]
            pub enum btree_update_mode: u32 {
                $($acc)*
                $([<BTREE_UPDATE_ $n>],)*
            }
        }
    } };
}
BTREE_UPDATE_MODES!(__btree_update_mode_0 []);

#[repr(C)]
#[derive(CStruct)]
pub struct btree_update_node {
    pub b: *mut c::btree,
    pub level: core::ffi::c_uint,
    pub root: bool,
    pub update_node_key: bool,
    #[c_anon("")] pub __seq_align: [u64; 0],
    pub seq: le::U64,
    pub key: c::bkey_i,
    pub key_pad: [u64; c::BKEY_BTREE_PTR_VAL_U64s_MAX],
}
c_default!(btree_update_node);

c_typedef! {
    #[c("DARRAY_PREALLOCATED(struct btree_update_node, BTREE_UPDATE_NODES_MAX) btree_update_nodes")]
    pub type btree_update_nodes = DArray<c::btree_update_node, { c::BTREE_UPDATE_NODES_MAX as usize }>;
}

#[bitfield(u8)]
pub struct btree_update_nodes_written_bits {
    #[bits(1)]
    pub nodes_written: u32,
    #[bits(1)]
    pub took_gc_lock: u32,
    #[bits(6)]
    pub __pad: u8,
}

#[repr(C)]
#[derive(CStruct)]
pub struct prealloc_nodes {
    pub b: [*mut c::btree; c::BTREE_UPDATE_NODES_MAX as usize],
    pub nr: core::ffi::c_uint,
    pub consumed: core::ffi::c_uint,
}
c_default!(prealloc_nodes);

/*
 * Tracks an in progress split/rewrite of a btree node and the update to the
 * parent node:
 *
 * When we split/rewrite a node, we do all the updates in memory without
 * waiting for any writes to complete - we allocate the new node(s) and update
 * the parent node, possibly recursively up to the root.
 *
 * The end result is that we have one or more new nodes being written -
 * possibly several, if there were multiple splits - and then a write (updating
 * an interior node) which will make all these new nodes visible.
 *
 * Additionally, as we split/rewrite nodes we free the old nodes - but the old
 * nodes can't be freed (their space on disk can't be reclaimed) until the
 * update to the interior node that makes the new node visible completes -
 * until then, the old nodes are still reachable on disk.
 *
 */
#[repr(C)]
#[derive(CStruct)]
pub struct btree_update {
    pub cl: c::closure,
    pub c: *mut c::bch_fs,
    pub start_time: u64,
    pub ip_started: core::ffi::c_ulong,

    pub list: c::list_head,
    pub unwritten_list: c::list_head,

    pub mode: c::btree_update_mode,
    pub flags: c::bch_trans_commit_flags,
    #[c_bitfield]
    pub nodes_written_bits: btree_update_nodes_written_bits,

    pub btree_id: c::btree_id,
    pub node_start: c::bpos,
    pub node_end: c::bpos,
    pub node_needed_rewrite: c::btree_node_rewrite_reason,
    pub node_written: u16,
    pub node_sectors: u16,
    pub node_remaining: u16,

    pub update_level_start: core::ffi::c_uint,
    pub update_level_end: core::ffi::c_uint,

    /* size of the key that triggered split_leaf (0 if N/A) — drives
     * the split-vs-compact decision in btree_split() so we don't loop
     * trying to compact a leaf that can't fit the new key.
     */
    pub new_key_u64s: core::ffi::c_uint,

    pub disk_res: c::disk_reservation,

    /*
     * BTREE_UPDATE_node:
     * The update that made the new nodes visible was a regular update to an
     * existing interior node - @b. We can't write out the update to @b
     * until the new nodes we created are finished writing, so we block @b
     * from writing by putting this btree_interior update on the
     * @b->write_blocked list with @write_blocked_list:
     */
    pub b: *mut c::btree,
    pub write_blocked_list: c::list_head,

    /*
     * We may be freeing nodes that were dirty, and thus had journal entries
     * pinned: we need to transfer the oldest of those pins to the
     * btree_update operation, and release it when the new node(s)
     * are all persistent and reachable:
     */
    pub journal: c::journal_entry_pin,

    /*
     * Preallocated nodes we reserve when we start the update.
     *
     * b[0..consumed) have been popped by bch2_btree_node_alloc and given
     * out to consumers (split/merge/rewrite/grow); b[consumed..nr) are
     * still in reserve.  bch2_btree_reserve_put walks both halves and
     * drops the as-owned intent+write refs uniformly — the consumed
     * half also runs path/state rollback (live → NONE, drops path
     * recurses).
     */
    pub prealloc_nodes: [c::prealloc_nodes; 2],

    pub old_nodes: c::btree_update_nodes,
    pub new_nodes: c::btree_update_nodes,

    #[c("open_bucket_idx_t open_buckets[BTREE_UPDATE_NODES_MAX * BCH_REPLICAS_MAX]")]
    pub open_buckets: [c::open_bucket_idx_t; c::BTREE_UPDATE_NODES_MAX as usize * c::BCH_REPLICAS_MAX as usize],
    pub nr_open_buckets: c::open_bucket_idx_t,

    /* Only here to reduce stack usage on recursive splits: */
    pub parent_keys: c::keylist,
    /*
     * Enough room for btree_split's keys without realloc - btree node
     * pointers never have crc/compression info, so we only need to acount
     * for the pointers for three keys
     */
    #[c("u64 inline_keys[BKEY_BTREE_PTR_U64s_MAX * 3]")]
    pub inline_keys: [u64; c::BKEY_BTREE_PTR_U64s_MAX * 3],
}
c_default!(btree_update);
impl btree_update {
    pub fn nodes_written(&self) -> u32 { let b = self.nodes_written_bits; b.nodes_written() }
    pub fn set_nodes_written(&mut self, v: u32) { let mut b = self.nodes_written_bits; b.set_nodes_written(v); self.nodes_written_bits = b; }
    pub fn took_gc_lock(&self) -> u32 { let b = self.nodes_written_bits; b.took_gc_lock() }
    pub fn set_took_gc_lock(&mut self, v: u32) { let mut b = self.nodes_written_bits; b.set_took_gc_lock(v); self.nodes_written_bits = b; }
}

c_enum! {
    #[closed]
    pub enum async_btree_op: u32 {
        ASYNC_BTREE_rewrite,
        ASYNC_BTREE_merge,
        ASYNC_BTREE_merge_no_read,
    }
}
