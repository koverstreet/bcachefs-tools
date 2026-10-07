// SPDX-License-Identifier: GPL-2.0

//! The data types of fs/inode_opts_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{CStruct, c_extern, c_verbatim};

c_verbatim!(r#"
struct bch_inode_unpacked;
struct bkey_i_logged_op_inode_opt_propagate;
"#);

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

c_verbatim!(r#"
struct bch_extent_reconcile;
"#);

// What Rust calls of fs/inode_opts.h: C gets these as prototypes, in fs/inode_opts_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    #[c("const char * const bch2_inode_opts[]")]
    pub static bch2_inode_opts: [*const crate::util::ffi::c_char; 0usize];
    pub fn bch2_opt_to_inode_opt(arg1: core::ffi::c_int) -> core::ffi::c_int;
    pub fn bch2_inode_opts_to_opts(arg1: *mut c::bch_inode_unpacked) -> c::bch_opts;
    pub fn bch2_reinherit_attrs(arg1: *mut c::bch_inode_unpacked, arg2: *mut c::bch_inode_unpacked) -> bool;
    pub fn bch2_inode_opt_change_init(arg1: *mut c::inode_opt_change);
    pub fn bch2_inode_opt_change_trans(arg1: *mut c::btree_trans, arg2: *mut c::bch_extent_reconcile, arg3: *mut c::bch_inode_unpacked, arg4: u32, arg5: *mut c::inode_opt_change) -> core::ffi::c_int;
    pub fn bch2_inode_opt_change_finish(arg1: *mut c::btree_trans, arg2: *mut c::inode_opt_change) -> core::ffi::c_int;
    pub fn bch2_check_inode_opts_propagated(arg1: *mut c::btree_trans, arg2: *mut c::bch_inode_unpacked) -> core::ffi::c_int;
}
