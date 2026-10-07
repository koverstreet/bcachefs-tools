// SPDX-License-Identifier: GPL-2.0

//! The data types of btree/journal_overlay_defs.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::CStruct;

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct journal_iter {
    pub list: c::list_head,
    pub btree_id: c::btree_id,
    pub level: core::ffi::c_uint,
    pub idx: usize,
    pub keys: *mut c::journal_keys,
}
c_default!(journal_iter);

/*
 * Iterate over keys in the btree, with keys from the journal overlaid on top:
 */
#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct btree_and_journal_iter {
    pub trans: *mut c::btree_trans,
    pub b: *mut c::btree,
    pub node_iter: c::btree_node_iter,
    pub unpacked: c::bkey,

    pub journal: c::journal_iter,
    pub pos: c::bpos,
    pub at_end: bool,
    pub prefetch: bool,
    pub fail_if_too_many_whiteouts: bool,
}
c_default!(btree_and_journal_iter);
