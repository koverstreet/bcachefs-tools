// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{DArray, c_default};
use cstruct_macros::{CStruct, c_typedef};

c_typedef! {
    pub type darray_bkey_i_backpointer = DArray<c::bkey_i_backpointer>;
}

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
