// SPDX-License-Identifier: GPL-2.0

//! The data types of btree/node_scan_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{DArray, c_default};
use cstruct_macros::{bitfield, c_verbatim, CStruct};

#[bitfield(u8)]
pub struct found_btree_node_range_updated_bits {
    #[bits(1)]
    pub range_updated: bool,
    #[bits(7)]
    pub __pad: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
#[c_typedef(found_btree_node)]
pub struct found_btree_node {
    #[c_bitfield]
    pub range_updated_bits: found_btree_node_range_updated_bits,
    pub btree_id: u8,
    pub level: u8,
    pub sectors_written: core::ffi::c_uint,
    pub seq: u32,
    pub journal_seq: u64,
    pub cookie: u64,

    pub min_key: c::bpos,
    pub max_key: c::bpos,

    pub nr_ptrs: core::ffi::c_uint,
    pub ptrs: [c::bch_extent_ptr; c::BCH_REPLICAS_MAX as usize],
}
impl found_btree_node {
    pub fn range_updated(&self) -> bool { let b = self.range_updated_bits; b.range_updated() }
    pub fn set_range_updated(&mut self, v: bool) { let mut b = self.range_updated_bits; b.set_range_updated(v); self.range_updated_bits = b; }
}

c_verbatim!(r#"
DEFINE_DARRAY(found_btree_node);
"#);

pub type darray_found_btree_node = DArray<c::found_btree_node>;

#[repr(C)]
#[derive(CStruct)]
pub struct find_btree_nodes {
    pub ret: core::ffi::c_int,
    pub lock: c::mutex,
    pub nodes: c::darray_found_btree_node,
}
c_default!(find_btree_nodes);
