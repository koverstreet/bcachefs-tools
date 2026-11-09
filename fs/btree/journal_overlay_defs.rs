// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::types::c_default;
use core::ffi::c_uint;
use cstruct_macros::CStruct;
use crate::format::{Bpos, Btree};

/*
 * A btree and level's journal keys, from @pos: at index @idx, as of the keys'
 * seq @seq - after an insert, searched for again
 */
#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct journal_iter {
    pub btree_id: Btree,
    pub level: c_uint,
    pub idx: usize,
    pub seq: u64,
    pub pos: Bpos,
}
c_default!(journal_iter);
