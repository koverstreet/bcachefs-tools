// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of snapshots/subvolume.h - C gets it as prototypes, in snapshots/subvolume_gen.h,
//! which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use core::ffi::{c_int, c_uint};
use cstruct_macros::c_extern;

c_extern! {
    pub fn bch2_subvolume_state_set(s: *mut c::bch_subvolume, n: c::bch_subvolume_state);
    pub fn bch2_subvolume_validate(c: *mut c::bch_fs, k: c::bkey_s_c, from: *const c::bkey_validate_context) -> c_int;
    pub fn bch2_subvol_has_children(trans: *mut c::btree_trans, subvol: u32) -> c_int;
    pub fn bch2_subvolume_get(trans: *mut c::btree_trans, subvol: c_uint, inconsistent_if_not_found: bool, s: *mut c::bch_subvolume) -> c_int;
    pub fn bch2_subvolume_get_key(trans: *mut c::btree_trans, subvol: c_uint, inconsistent_if_not_found: bool, k: *mut c::bkey_i_subvolume) -> c_int;
    pub fn bch2_subvolume_is_unlinked(trans: *mut c::btree_trans, subvolid: u32) -> c_int;
    pub fn __bch2_subvolume_get_snapshot(trans: *mut c::btree_trans, subvolid: u32, snapid: *mut u32, warn: bool) -> c_int;
    pub fn bch2_subvolume_get_snapshot(trans: *mut c::btree_trans, subvolid: u32, snapid: *mut u32) -> c_int;
    pub fn bch2_subvol_is_ro_trans(trans: *mut c::btree_trans, subvol: u32, snapid: *mut u32) -> c_int;
    pub fn bch2_subvol_is_ro(c: *mut c::bch_fs, subvol: u32) -> c_int;
    pub fn bch2_subvolume_unlink(trans: *mut c::btree_trans, subvolid: u32) -> c_int;
    pub fn bch2_subvolume_create(trans: *mut c::btree_trans, inode: u64, parent_subvolid: u32, src_subvolid: u32, new_subvolid: *mut u32, new_snapshotid: *mut u32, new_subvol_out: *mut c::bch_subvolume, ro: bool) -> c_int;
}
