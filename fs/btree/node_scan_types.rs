// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{DArray, c_default};
use core::ffi::{c_int, c_uint};
use cstruct_macros::{bitfield, CStruct, c_typedef};
use nestify::nest;

nest! {
    #[repr(C)]
    #[derive(Clone, Copy, Default, CStruct)]
    #[c_typedef(found_btree_node)]
    pub struct found_btree_node {
        #[c_bitfield]
        #>[bitfield(u8)]
        pub bits: pub struct found_btree_node_bits {
            #[bits(1)]
            pub range_updated: bool,
            #[bits(7)]
            pub __pad: u8,
        },
        pub btree_id: u8,
        pub level: u8,
        pub sectors_written: c_uint,
        pub seq: u32,
        pub journal_seq: u64,
        pub cookie: u64,

        pub min_key: c::bpos,
        pub max_key: c::bpos,

        pub nr_ptrs: c_uint,
        pub ptrs: [c::bch_extent_ptr; c::BCH_REPLICAS_MAX as usize],
    }
}

c_typedef! {
    pub type darray_found_btree_node = DArray<c::found_btree_node>;
}

#[repr(C)]
#[derive(CStruct)]
pub struct find_btree_nodes {
    pub ret: c_int,
    pub lock: c::mutex,
    pub nodes: c::darray_found_btree_node,
}
c_default!(find_btree_nodes);
