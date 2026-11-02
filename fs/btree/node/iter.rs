// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c as cs;
use cstruct_macros::CStruct;

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct btree_node_iter_set {
    pub k: u16,
    pub end: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct btree_node_iter {
    pub data: [cs::btree_node_iter_set; cs::MAX_BSETS as usize],
}
