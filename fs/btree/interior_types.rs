// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use core::ffi::{c_int, c_uint};
use cstruct_macros::{CStruct, c_const, c_extern};
use typeinfo_macros::TypeInfo;

#[repr(C)]
#[derive(CStruct)]
pub struct btree_alloc {
    pub ob: c::open_buckets,
    pub k: c::bkey_i,
    pub k_pad: [u64; c::BKEY_BTREE_PTR_VAL_U64s_MAX],
}
c_default!(btree_alloc);

c_const! {
    /* Maximum number of nodes we might need to allocate atomically: */
    #[c_int]
    pub const BTREE_RESERVE_MAX: u32 = c::BTREE_MAX_DEPTH as u32 + (c::BTREE_MAX_DEPTH as u32 - 1);

    /* Size of the freelist we allocate btree nodes from: */
    #[c_int]
    pub const BTREE_NODE_RESERVE: u32 = c::BTREE_RESERVE_MAX * 4;
}

/*
 * Cache of allocated btree nodes - if we allocate a btree node and don't use
 * it, if we free it that space can't be reused until going _all_ the way
 * through the allocator (which exposes us to a livelock when allocating btree
 * reserves fail halfway through) - instead, we can stick them here:
 */
#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_fs_btree_reserve_cache {
    pub lock: c::mutex,
    pub nr: c_uint,
    #[c("struct btree_alloc data[BTREE_NODE_RESERVE * 2]")]
    pub data: [c::btree_alloc; c::BTREE_NODE_RESERVE as usize * 2],
}
c_default!(bch_fs_btree_reserve_cache);

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_fs_btree_interior_updates {
    pub pool: c::mempool_t,
    pub list: c::list_head,
    pub unwritten: c::list_head,
    pub lock: c::mutex,
    pub commit_lock: c::mutex,
    pub wait: c::closure_waitlist,

    pub worker: *mut c::workqueue_struct,
    pub work: c::work_struct,
}
c_default!(bch_fs_btree_interior_updates);

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_fs_btree_node_rewrites {
    pub list: c::list_head,
    pub pending: c::list_head,
    pub lock: c::spinlock_t,
    pub wait: c::closure_waitlist,
    pub nr: c::atomic_t,
    pub table: c::rhashtable,
    pub table_init_done: bool,
    pub worker: *mut c::workqueue_struct,
}
c_default!(bch_fs_btree_node_rewrites);

c_extern! {
    pub fn bch2_btree_node_update_key(trans: *mut c::btree_trans, iter: *mut c::btree_iter, b: *mut c::btree, new_key: *mut c::bkey_i, commit_flags: c_uint, skip_triggers: bool) -> c_int;
}
