// SPDX-License-Identifier: GPL-2.0

//! The data types of util/seqmutex_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

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
