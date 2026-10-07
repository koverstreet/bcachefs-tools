// SPDX-License-Identifier: GPL-2.0

//! The data types of alloc/buckets_defs.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{c_enum, CStruct};

c_enum! {
    #[flags]
    pub enum bch_reservation_flags: u32 {
        BCH_DISK_RESERVATION_NOFAIL = 1 << 0,
        BCH_DISK_RESERVATION_PARTIAL = 1 << 1,
    }
}

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct disk_reservation_destructable {
    pub c: *mut c::bch_fs,
    pub r: c::disk_reservation,
}
c_default!(disk_reservation_destructable);
