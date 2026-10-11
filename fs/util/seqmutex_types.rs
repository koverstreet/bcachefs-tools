// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::CStruct;

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct seqmutex {
    pub lock: c::mutex,
    pub seq: u32,
}
c_default!(seqmutex);
