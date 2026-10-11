// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use crate::util::ffi::c_char;
use core::ffi::{c_int, c_uint};
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
    pub nr: c_uint,
    #[c("struct bch_disk_group_cpu entries[] __counted_by(nr)")]
    pub entries: [c::bch_disk_group_cpu; 0],
}
c_default!(bch_disk_groups_cpu);

c_extern! {
    pub fn bch2_disk_path_find(sb: *mut c::bch_sb_handle, name: *const c_char) -> c_int;
    pub fn bch2_disk_path_find_or_create(sb: *mut c::bch_sb_handle, name: *const c_char) -> c_int;
}
