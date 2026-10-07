// SPDX-License-Identifier: GPL-2.0

//! The data types of btree/key_cache_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::CStruct;
use typeinfo_macros::TypeInfo;

#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_fs_btree_key_cache {
    pub table: c::rhashtable,
    pub table_init_done: bool,

    pub shrink: *mut c::shrinker,
    pub shrink_iter: core::ffi::c_uint,

    /* 0: non pcpu reader locks, 1: pcpu reader locks */
    pub pending: [c::rcu_pending; 2],
    #[c("size_t __percpu *nr_pending")]
    pub nr_pending: *mut usize,

    pub nr_keys: c::atomic_long_t,
    pub nr_dirty: c::atomic_long_t,

    /* shrinker stats */
    pub requested_to_free: core::ffi::c_ulong,
    pub freed: core::ffi::c_ulong,
    pub skipped_dirty: core::ffi::c_ulong,
    pub skipped_accessed: core::ffi::c_ulong,
    pub skipped_lock_fail: core::ffi::c_ulong,
}
c_default!(bch_fs_btree_key_cache);

#[repr(C, align(4))]
#[derive(Clone, Copy, Default, CStruct)]
#[c_packed]
pub struct bkey_cached_key {
    pub btree_id: u32,
    pub pos: c::bpos,
}
