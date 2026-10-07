// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of snapshots/subvolume.h - C gets it as prototypes, in snapshots/subvolume_gen.h,
//! which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::c_extern;

// What Rust calls of snapshots/subvolume.h: C gets these as prototypes, in snapshots/subvolume_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_subvolume_state_set(arg1: *mut c::bch_subvolume, arg2: c::bch_subvolume_state);
    pub fn bch2_subvolume_validate(arg1: *mut c::bch_fs, arg2: c::bkey_s_c, arg3: *const c::bkey_validate_context) -> core::ffi::c_int;
    pub fn bch2_subvol_has_children(arg1: *mut c::btree_trans, arg2: u32) -> core::ffi::c_int;
    pub fn bch2_subvolume_get(arg1: *mut c::btree_trans, arg2: core::ffi::c_uint, arg3: bool, arg4: *mut c::bch_subvolume) -> core::ffi::c_int;
    pub fn bch2_subvolume_get_key(arg1: *mut c::btree_trans, arg2: core::ffi::c_uint, arg3: bool, arg4: *mut c::bkey_i_subvolume) -> core::ffi::c_int;
    pub fn bch2_subvolume_is_unlinked(arg1: *mut c::btree_trans, arg2: u32) -> core::ffi::c_int;
    pub fn __bch2_subvolume_get_snapshot(arg1: *mut c::btree_trans, arg2: u32, arg3: *mut u32, arg4: bool) -> core::ffi::c_int;
    pub fn bch2_subvolume_get_snapshot(arg1: *mut c::btree_trans, arg2: u32, arg3: *mut u32) -> core::ffi::c_int;
    pub fn bch2_subvol_is_ro_trans(arg1: *mut c::btree_trans, arg2: u32, arg3: *mut u32) -> core::ffi::c_int;
    pub fn bch2_subvol_is_ro(arg1: *mut c::bch_fs, arg2: u32) -> core::ffi::c_int;
    pub fn bch2_subvolume_unlink(arg1: *mut c::btree_trans, arg2: u32) -> core::ffi::c_int;
    pub fn bch2_subvolume_create(arg1: *mut c::btree_trans, arg2: u64, arg3: u32, arg4: u32, arg5: *mut u32, arg6: *mut u32, arg7: *mut c::bch_subvolume, arg8: bool) -> core::ffi::c_int;
}
