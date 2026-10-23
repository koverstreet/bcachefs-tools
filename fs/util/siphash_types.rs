// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::types::Le64;
use cstruct_macros::{CStruct, Plain};

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain)]
#[c_typedef]
pub struct SIPHASH_KEY {
    pub k0: Le64,
    pub k1: Le64,
}
