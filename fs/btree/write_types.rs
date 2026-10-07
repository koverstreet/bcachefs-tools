// SPDX-License-Identifier: GPL-2.0

//! The data types of btree/write_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{c_enum, CStruct};

#[repr(C)]
#[derive(CStruct)]
pub struct btree_write_bio {
    pub work: c::work_struct,
    pub key: c::bkey_i,
    pub key_pad: [u64; c::BKEY_BTREE_PTR_VAL_U64s_MAX],
    pub data: *mut core::ffi::c_void,
    pub data_bytes: core::ffi::c_uint,
    pub sector_offset: core::ffi::c_uint,
    pub start_time: u64,
    /* C's CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS: see types.rs */
    #[cfg(all(__KERNEL__, CONFIG_DEBUG_FS))]
    pub list_idx: core::ffi::c_uint,
    pub wbio: c::bch_write_bio,
}
c_default!(btree_write_bio);

c_enum! {
    #[closed]
    pub enum btree_write_flags: u32 {
        __BTREE_WRITE_only_if_need = c::BTREE_WRITE_TYPE_BITS,
        __BTREE_WRITE_already_started,
    }
}
