// SPDX-License-Identifier: GPL-2.0

//! The data types of btree/write_buffer_defs.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{c_verbatim, CStruct};

c_verbatim!(r#"
struct btree_trans;
"#);

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct wb_maybe_flush {
    pub last_flushed: c::bkey_buf,
    pub flushed_commit_count: u32,
    pub nr_flushes: u64,
    pub nr_done: u64,
    pub seen_error: bool,
}

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct journal_keys_to_wb_btree {
    pub wb: *mut c::btree_write_buffer_keys, /* NULL: not yet acquired */
    pub room: usize,
}
c_default!(journal_keys_to_wb_btree);

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct journal_keys_to_wb {
    pub seq: u64,
    #[cfg(CONFIG_BCACHEFS_TESTS)]
    pub test_wb_pin_armed: bool,
    pub per_btree: [c::journal_keys_to_wb_btree; c::BCH_WB_BTREE_NR as usize],
}
