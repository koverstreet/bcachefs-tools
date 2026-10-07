// SPDX-License-Identifier: GPL-2.0

//! The data types of init/progress_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{CStruct, c_extern};

/*
 * Lame progress indicators
 *
 * We don't like to use these because they print to the dmesg console, which is
 * spammy - we much prefer to be wired up to a userspace programm (e.g. via
 * thread_with_file) and have it print the progress indicator.
 *
 * But some code is old and doesn't support that, or runs in a context where
 * that's not yet practical (mount).
 */
#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct progress_indicator {
    pub msg: *const crate::util::ffi::c_char,
    pub units: c::bch_progress_units,
    pub pos: c::bbpos,
    pub next_print: core::ffi::c_ulong,
    pub seen: u64,
    pub total: u64,
    pub last_node: *mut c::btree,
    pub silent: bool,
}
c_default!(progress_indicator);

// What Rust calls of init/progress.h: C gets these as prototypes, in init/progress_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_progress_init(s: *mut c::progress_indicator, msg: *const crate::util::ffi::c_char, c: *mut c::bch_fs, leaf_btree_id_mask: u64, inner_btree_id_mask: u64);
    pub fn bch2_progress_update_iter(arg1: *mut c::btree_trans, arg2: *mut c::progress_indicator, arg3: *mut c::btree_iter) -> core::ffi::c_int;
}
