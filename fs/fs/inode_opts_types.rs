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
    pub fn bch2_opt_to_inode_opt(id: c_int) -> c_int;
    pub fn bch2_inode_opts_get_inode(c: *mut c::bch_fs, inode: *mut c::bch_inode_unpacked,
                                     ret: *mut c::bch_inode_opts);
    pub fn bch2_inode_opts_to_opts(inode: *mut c::bch_inode_unpacked) -> c::bch_opts;
    pub fn bch2_reinherit_attrs(dst_u: *mut c::bch_inode_unpacked,
                                src_u: *mut c::bch_inode_unpacked) -> bool;
    pub fn bch2_inode_opt_change_init(ch: *mut c::inode_opt_change);
    pub fn bch2_inode_opt_change_trans(trans: *mut c::btree_trans,
                                       old_r: *mut c::bch_extent_reconcile,
                                       inode_u: *mut c::bch_inode_unpacked, snapshot: u32,
                                       ch: *mut c::inode_opt_change) -> c_int;
    pub fn bch2_inode_opt_change_finish(trans: *mut c::btree_trans,
                                        ch: *mut c::inode_opt_change) -> c_int;
    pub fn bch2_check_inode_opts_propagated(trans: *mut c::btree_trans,
                                            inode: *mut c::bch_inode_unpacked) -> c_int;
}
