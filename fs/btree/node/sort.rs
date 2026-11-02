// SPDX-License-Identifier: GPL-2.0

use super::BtreeNode;
use super::bset::MAX_BSETS;
use crate::cstructs::c;
use crate::types::c_default;
use core::ffi::c_uint;
use cstruct_macros::{c_enum, c_name, CStruct};

#[repr(C)]
#[c_name(sort_iter_set)]
#[derive(Clone, Copy, CStruct)]
pub struct SortIterSet {
    pub k:   *mut c::bkey_packed,
    pub end: *mut c::bkey_packed,
}
c_default!(SortIterSet);

#[repr(C)]
#[c_name(sort_iter)]
#[derive(Clone, Copy, CStruct)]
pub struct SortIter {
    pub b:    *mut BtreeNode,
    pub used: c_uint,
    pub size: c_uint,

    pub data: [SortIterSet; 0],
}
c_default!(SortIter);

#[repr(C)]
#[c_name(sort_iter_stack)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct SortIterStack {
    pub iter: SortIter,
    #[c("struct sort_iter_set sets[MAX_BSETS + 1]")]
    pub sets: [SortIterSet; MAX_BSETS as usize + 1],
}

c_enum! {
    #[closed]
    #[c_name(compact_mode)]
    pub enum CompactMode: u32 {
        COMPACT_LAZY,
        COMPACT_ALL,
    }
}
