// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::MaybeZeroable;

#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable)]
pub struct wb_maybe_flush {
    pub last_flushed: c::bkey_buf,
    pub flushed_commit_count: u32,
    pub nr_flushes: u64,
    pub nr_done: u64,
    pub seen_error: bool,
}
