// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::c_const;

c_const! {
    #[c_int]
    pub const BUCKET_NOCOW_LOCKS_BITS: u32 = 10;
    pub const BUCKET_NOCOW_LOCKS: u32 = 1 << c::BUCKET_NOCOW_LOCKS_BITS;

    #[c_int]
    pub const NOCOW_LOCK_BUCKET_SIZE: u32 = 6;
}
