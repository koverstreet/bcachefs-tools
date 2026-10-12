// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{Le32, Le64};
use cstruct_macros::{c_bitmask, CStruct, TypeInfo};

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_reflink_p {
    pub v: c::bch_val,
    pub idx_flags: Le64,
    /*
     * A reflink pointer might point to an indirect extent which is then
     * later split (by copygc or rebalance). If we only pointed to part of
     * the original indirect extent, and then one of the fragments is
     * outside the range we point to, we'd leak a refcount: so when creating
     * reflink pointers, we need to store pad values to remember the full
     * range we were taking a reference on.
     */
    pub front_pad: Le32,
    pub back_pad: Le32,
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
    pub refcount: Le64,
    pub start: [c::bch_extent_entry; 0],
    pub _data: [u64; 0],
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_indirect_inline_data {
    pub v: c::bch_val,
    pub refcount: Le64,
    pub data: [u8; 0],
}
