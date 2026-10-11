// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use crate::util::ffi::c_char;
use core::ffi::c_int;
use cstruct_macros::{CStruct, c_extern};

/*
 * What changing an inode's options leaves for after the commit: an inode
 * option change has to reach existing data (a reconcile scan) and older
 * snapshots of the inode (the propagate logged op, finished after commit).
 */
#[repr(C)]
#[derive(CStruct)]
pub struct inode_opt_change {
    pub reconcile_changed: bool,
    pub propagate: c::bkey_i_logged_op_inode_opt_propagate,
}
c_default!(inode_opt_change);

c_extern! {
    pub static bch2_inode_opts: [*const c_char; 0];
    pub fn bch2_opt_to_inode_opt(arg1: c_int) -> c_int;
    pub fn bch2_inode_opts_get_inode(arg1: *mut c::bch_fs, arg2: *mut c::bch_inode_unpacked, arg3: *mut c::bch_inode_opts);
    pub fn bch2_inode_opts_to_opts(arg1: *mut c::bch_inode_unpacked) -> c::bch_opts;
    pub fn bch2_reinherit_attrs(arg1: *mut c::bch_inode_unpacked, arg2: *mut c::bch_inode_unpacked) -> bool;
    pub fn bch2_inode_opt_change_init(arg1: *mut c::inode_opt_change);
    pub fn bch2_inode_opt_change_trans(arg1: *mut c::btree_trans, arg2: *mut c::bch_extent_reconcile, arg3: *mut c::bch_inode_unpacked, arg4: u32, arg5: *mut c::inode_opt_change) -> c_int;
    pub fn bch2_inode_opt_change_finish(arg1: *mut c::btree_trans, arg2: *mut c::inode_opt_change) -> c_int;
    pub fn bch2_check_inode_opts_propagated(arg1: *mut c::btree_trans, arg2: *mut c::bch_inode_unpacked) -> c_int;
}
