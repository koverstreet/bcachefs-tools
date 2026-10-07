// SPDX-License-Identifier: GPL-2.0

//! The data types of alloc/backpointers_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{DArray, c_default};
use cstruct_macros::{c_verbatim, CStruct};

c_verbatim!(r#"
struct wb_maybe_flush;

DEFINE_DARRAY_NAMED(darray_bkey_i_backpointer, struct bkey_i_backpointer);
"#);

pub type darray_bkey_i_backpointer = DArray<c::bkey_i_backpointer>;

c_verbatim!(r#"
struct progress_indicator;
"#);

#[repr(C)]
#[derive(CStruct)]
pub struct bp_scan_iter {
    /* BTREE_ID_backpointers, BTREE_ID_stripe_backpointers */
    pub btree: c::btree_id,
    pub pos: c::bpos,
    pub nr_flushes: u64,
    pub progress: *mut c::progress_indicator,
    pub bps: c::darray_bkey_i_backpointer,
}
c_default!(bp_scan_iter);
