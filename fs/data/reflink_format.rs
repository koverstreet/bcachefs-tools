// SPDX-License-Identifier: GPL-2.0

//! The data types of data/reflink_format.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use zerocopy::byteorder::little_endian as le;
use cstruct_macros::{c_bitmask, CStruct};
use typeinfo_macros::TypeInfo;

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_reflink_p {
    pub v: c::bch_val,
    pub idx_flags: le::U64,
    /*
     * A reflink pointer might point to an indirect extent which is then
     * later split (by copygc or rebalance). If we only pointed to part of
     * the original indirect extent, and then one of the fragments is
     * outside the range we point to, we'd leak a refcount: so when creating
     * reflink pointers, we need to store pad values to remember the full
     * range we were taking a reference on.
     */
    pub front_pad: le::U32,
    pub back_pad: le::U32,
}

c_bitmask! {
    LE64_BITMASK(struct bch_reflink_p, idx_flags), strip REFLINK_P_ {
        REFLINK_P_IDX(0, 56),
        REFLINK_P_ERROR(56, 57),
        REFLINK_P_MAY_UPDATE_OPTIONS(57, 58),
    }
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_reflink_v {
    pub v: c::bch_val,
    pub refcount: le::U64,
    pub start: [c::bch_extent_entry; 0],
    pub _data: [u64; 0],
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_indirect_inline_data {
    pub v: c::bch_val,
    pub refcount: le::U64,
    pub data: [u8; 0],
}
