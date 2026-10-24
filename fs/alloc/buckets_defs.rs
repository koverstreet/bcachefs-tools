// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::{c_enum, CStruct};

c_enum! {
    #[flags]
    pub enum bch_reservation_flags: u32 {
        BCH_DISK_RESERVATION_NOFAIL = 1 << 0,
        BCH_DISK_RESERVATION_PARTIAL = 1 << 1,
    }
}

#[repr(C)]
#[derive(CStruct)]
pub struct disk_reservation_destructable {
    pub r: c::disk_reservation,
}
