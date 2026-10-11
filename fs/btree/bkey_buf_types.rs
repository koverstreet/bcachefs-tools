// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::CStruct;

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct bkey_buf {
    pub k: *mut c::bkey_i,
    pub onstack: [u64; 12],
}
c_default!(bkey_buf);
