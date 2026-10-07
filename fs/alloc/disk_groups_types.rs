// SPDX-License-Identifier: GPL-2.0

//! The data types of alloc/disk_groups_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{CStruct, c_extern};
use typeinfo_macros::TypeInfo;

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_disk_group_cpu {
    pub deleted: bool,
    pub parent: u16,
    pub label: [u8; c::BCH_SB_LABEL_SIZE as usize],
    pub devs: c::bch_devs_mask,
}

#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_disk_groups_cpu {
    pub rcu: c::rcu_head,
    pub nr: core::ffi::c_uint,
    #[c("struct bch_disk_group_cpu entries[] __counted_by(nr)")]
    pub entries: [c::bch_disk_group_cpu; 0],
}
c_default!(bch_disk_groups_cpu);

// What Rust calls of alloc/disk_groups.h: C gets these as prototypes, in alloc/disk_groups_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_disk_path_find(arg1: *mut c::bch_sb_handle, arg2: *const crate::util::ffi::c_char) -> core::ffi::c_int;
    pub fn bch2_disk_path_find_or_create(arg1: *mut c::bch_sb_handle, arg2: *const crate::util::ffi::c_char) -> core::ffi::c_int;
}
