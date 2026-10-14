// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of snapshots/snapshot.h - C gets it as prototypes, in
//! snapshots/snapshot_gen.h, which is generated from this file: see
//! fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::btree::bkey::BkeyI;
use crate::cstructs::c;
use crate::util::ffi::c_char;
use core::ffi::{c_int, c_uint};
use cstruct_macros::c_extern;

c_extern! {
    pub fn __bch2_snapshot_tree_create(trans: *mut c::btree_trans) -> *mut c::bkey_i_snapshot_tree;
    pub fn bch2_snapshot_tree_lookup(trans: *mut c::btree_trans, id: u32,
                                     s: *mut c::bch_snapshot_tree) -> c_int;
    pub fn bch2_snapshot_to_text(out: *mut c::printbuf, s: *const c::bch_snapshot);
    pub fn bch2_snapshot_state_str(s: c::bch_snapshot_state) -> *const c_char;
    pub fn bch2_snapshot_state_set(s: *mut c::bch_snapshot, n: c::bch_snapshot_state);
    pub fn bch2_snapshot_table_make_room(c: *mut c::bch_fs, id: u32) -> c_int;
    pub fn bch2_snapshot_skiplist_get(c: *mut c::bch_fs, id: u32) -> u32;
    pub fn bch2_snapshot_oldest_subvol(c: *mut c::bch_fs, root: u32) -> u32;
    pub fn bch2_snapshot_table_find_edge(c: *mut c::bch_fs, s: *const c::bch_snapshot, id: u32,
                                         parent: bool) -> u32;
    pub fn bch2_snapshot_redundant_interior(c: *mut c::bch_fs, id: u32) -> u32;
    pub fn __bch2_snapshot_is_ancestor(trans: *mut c::btree_trans, id: u32, ancestor: u32) -> bool;
    pub fn bch2_snapshot_is_ancestor_early(c: *mut c::bch_fs, id: u32, ancestor: u32) -> bool;
    pub fn bch2_snapshot_lookup(trans: *mut c::btree_trans, id: u32,
                                s: *mut c::bch_snapshot) -> c_int;
    pub fn bch2_snapshot_lookup_key(trans: *mut c::btree_trans, id: u32,
                                    k: *mut BkeyI<c::bch_snapshot>) -> c_int;
    pub fn bch2_snapshot_node_create(trans: *mut c::btree_trans, parent: u32, new_snapids: *mut u32,
                                     snapshot_subvols: *mut u32, nr_snapids: c_uint) -> c_int;
    pub fn bch2_snapshot_node_set_deleted(trans: *mut c::btree_trans, id: u32) -> c_int;
    pub fn bch2_snapshot_node_undelete(trans: *mut c::btree_trans,
                                       u: *mut BkeyI<c::bch_snapshot>) -> c_int;
    pub fn bch2_snapshot_accounting_totals(c: *mut c::bch_fs, id: u32, total_keys: *mut u64,
                                           total_sectors: *mut u64, btrees_with_keys: *mut u64,
                                           breakdown: *mut c::printbuf) -> c_int;
    pub fn bch2_delete_dead_snapshot_key(trans: *mut c::btree_trans, iter: *mut c::btree_iter,
                                         k: c::bkey_s_c, live_child: u32) -> c_int;
    pub fn bch2_snapshot_table_rebuild(trans: *mut c::btree_trans) -> c_int;
}
