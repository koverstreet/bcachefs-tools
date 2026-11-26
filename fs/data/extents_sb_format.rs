// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use cstruct_macros::{whole, MaybeZeroable, Plain, TypeInfo};

#[whole(tail = d: [u8])]
#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo)]
pub struct SbFieldExtentTypeU64s {
    pub d: [u8; 0],
}
