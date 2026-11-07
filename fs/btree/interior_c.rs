// SPDX-License-Identifier: GPL-2.0

//! interior.h's interior update machinery, which InteriorUpdate wraps.

use core::ffi::{c_int, c_uint};
use cstruct_macros::c_extern;
use crate::btree::iter::BtreeTrans;
use crate::btree::node::BtreeNode;
use crate::btree::path::{BtreePath, PathIdx};
use crate::c;

c_extern! {
    pub fn bch2_btree_update_start(trans: *mut BtreeTrans<'_>, path: PathIdx, level_start: c_uint, split: bool,
                                   target: c_uint, commit_flags: c::bch_trans_commit_flags,
                                   write_flags: c::bch_write_flags) -> *mut c::btree_update;
    pub fn bch2_btree_update_free(as_: *mut c::btree_update, trans: *mut BtreeTrans<'_>, bailed: bool);
    pub fn bch2_btree_update_done(as_: *mut c::btree_update, trans: *mut BtreeTrans<'_>);

    pub fn bch2_btree_node_alloc(as_: *mut c::btree_update, trans: *mut BtreeTrans<'_>,
                                 level: c_uint) -> *mut BtreeNode;
    pub fn bch2_btree_node_alloc_replacement(as_: *mut c::btree_update, trans: *mut BtreeTrans<'_>,
                                             b: *mut BtreeNode) -> *mut BtreeNode;
    pub fn __btree_root_alloc(as_: *mut c::btree_update, trans: *mut BtreeTrans<'_>,
                              level: c_uint) -> *mut BtreeNode;
    pub fn btree_path_take_new_node(trans: *mut BtreeTrans<'_>, path: *mut BtreePath, b: *mut BtreeNode);

    pub fn bch2_btree_update_emit_new_node_key(as_: *mut c::btree_update, b: *mut BtreeNode);
    pub fn bch2_btree_update_add_key(nodes: *mut c::btree_update_nodes, level: c_uint, k: *mut c::bkey_i);
    pub fn bch2_btree_set_root(as_: *mut c::btree_update, trans: *mut BtreeTrans<'_>,
                               path: *mut BtreePath, b: *mut BtreeNode);
    pub fn bch2_btree_insert_node(as_: *mut c::btree_update, trans: *mut BtreeTrans<'_>, path: PathIdx,
                                  b: *mut BtreeNode, keys: *mut c::keylist) -> c_int;

    pub fn bch2_btree_interior_update_will_free_node(as_: *mut c::btree_update, b: *mut BtreeNode);
    pub fn bch2_btree_update_write_new_node(as_: *mut c::btree_update, trans: *mut BtreeTrans<'_>,
                                            b: *mut BtreeNode);
    pub fn bch2_btree_node_free_inmem(trans: *mut BtreeTrans<'_>, path: *mut BtreePath, b: *mut BtreeNode);
}
