// SPDX-License-Identifier: GPL-2.0

use crate::cstructs::c as cs;
use cstruct_macros::{c_name, CStruct};

#[repr(C)]
#[c_name(btree_node_iter_set)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct KeyIterSet {
    pub k: u16,
    pub end: u16,
}

#[repr(C)]
#[c_name(btree_node_iter)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct KeyIter {
    pub data: [KeyIterSet; cs::MAX_BSETS as usize],
}
