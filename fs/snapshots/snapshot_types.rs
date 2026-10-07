// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of snapshots/snapshot.h - C gets it as prototypes, in snapshots/snapshot_gen.h,
//! which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::c_extern;

// What Rust calls of snapshots/snapshot.h: C gets these as prototypes, in snapshots/snapshot_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn __bch2_snapshot_tree_create(arg1: *mut c::btree_trans) -> *mut c::bkey_i_snapshot_tree;
    pub fn bch2_snapshot_tree_lookup(arg1: *mut c::btree_trans, arg2: u32, arg3: *mut c::bch_snapshot_tree) -> core::ffi::c_int;
    pub fn bch2_snapshot_to_text(arg1: *mut c::printbuf, arg2: *const c::bch_snapshot);
    pub fn bch2_snapshot_validate(arg1: *mut c::bch_fs, arg2: c::bkey_s_c, arg3: *const c::bkey_validate_context) -> core::ffi::c_int;
    pub fn bch2_snapshot_state_str(arg1: c::bch_snapshot_state) -> *const crate::util::ffi::c_char;
    pub fn bch2_snapshot_state_set(arg1: *mut c::bch_snapshot, arg2: c::bch_snapshot_state);
    pub fn bch2_snapshot_table_make_room(arg1: *mut c::bch_fs, arg2: u32) -> core::ffi::c_int;
    pub fn bch2_snapshot_skiplist_get(arg1: *mut c::bch_fs, arg2: u32) -> u32;
    pub fn bch2_snapshot_oldest_subvol(arg1: *mut c::bch_fs, arg2: u32) -> u32;
    pub fn bch2_snapshot_table_find_edge(arg1: *mut c::bch_fs, arg2: *const c::bch_snapshot, arg3: u32, arg4: bool) -> u32;
    pub fn bch2_snapshot_redundant_interior(arg1: *mut c::bch_fs, arg2: u32) -> u32;
    pub fn __bch2_snapshot_is_ancestor(arg1: *mut c::btree_trans, arg2: u32, arg3: u32) -> bool;
    pub fn bch2_snapshot_is_ancestor_early(arg1: *mut c::bch_fs, arg2: u32, arg3: u32) -> bool;
    pub fn bch2_snapshot_id_list_to_text(arg1: *mut c::printbuf, arg2: *mut c::snapshot_id_list);
    pub fn bch2_snapshot_lookup(trans: *mut c::btree_trans, id: u32, s: *mut c::bch_snapshot) -> core::ffi::c_int;
    pub fn bch2_snapshot_lookup_key(trans: *mut c::btree_trans, id: u32, k: *mut c::bkey_i_snapshot) -> core::ffi::c_int;
    pub fn bch2_snapshot_node_create(arg1: *mut c::btree_trans, arg2: u32, arg3: *mut u32, arg4: *mut u32, arg5: core::ffi::c_uint) -> core::ffi::c_int;
    pub fn bch2_check_snapshot_trees(arg1: *mut c::bch_fs) -> core::ffi::c_int;
    pub fn bch2_check_snapshots_trans(arg1: *mut c::btree_trans) -> core::ffi::c_int;
    pub fn bch2_check_snapshots(arg1: *mut c::bch_fs) -> core::ffi::c_int;
    pub fn bch2_reconstruct_snapshots(arg1: *mut c::bch_fs) -> core::ffi::c_int;
    pub fn __bch2_check_key_has_snapshot(arg1: *mut c::btree_trans, arg2: *mut c::btree_iter, arg3: c::bkey_s_c) -> core::ffi::c_int;
    pub fn bch2_snapshot_node_set_deleted(arg1: *mut c::btree_trans, arg2: u32) -> core::ffi::c_int;
    pub fn bch2_snapshot_node_delete(arg1: *mut c::btree_trans, arg2: u32) -> core::ffi::c_int;
    pub fn bch2_snapshot_node_undelete(arg1: *mut c::btree_trans, arg2: *mut c::bkey_i_snapshot) -> core::ffi::c_int;
    pub fn bch2_snapshot_accounting_totals(arg1: *mut c::bch_fs, arg2: u32, arg3: *mut u64, arg4: *mut u64, arg5: *mut u64, arg6: *mut c::printbuf) -> core::ffi::c_int;
    pub fn bch2_delete_dead_snapshot_key(arg1: *mut c::btree_trans, arg2: *mut c::btree_iter, arg3: c::bkey_s_c, arg4: u32) -> core::ffi::c_int;
    pub fn bch2_snapshot_table_rebuild(arg1: *mut c::btree_trans) -> core::ffi::c_int;
}
