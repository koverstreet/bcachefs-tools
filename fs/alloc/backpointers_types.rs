// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::btree::bkey::BkeyI;
use crate::cstructs::c;
use crate::util::alloc::KVVec;
use cstruct_macros::CStruct;

/// A backpointer scan in progress - alloc/backpointers.rs's, which C's
/// backpointer_scan_for_each() holds and inits through Rust: the batch is
/// Rust's alone. C reads pos, for its progress.
#[repr(C)]
#[derive(CStruct)]
pub struct bp_scan_iter {
    /* BTREE_ID_backpointers, BTREE_ID_stripe_backpointers */
    pub btree: c::btree_id,
    pub pos: c::bpos,
    pub nr_flushes: u64,
    pub progress: *mut c::progress_indicator,
    pub batch: KVVec<BkeyI<c::Backpointer>>,
}
