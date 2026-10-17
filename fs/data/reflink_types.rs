// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of data/reflink.h - C gets it as prototypes, in
//! data/reflink_gen.h, which is generated from this file: see
//! fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use core::ffi::c_int;
use cstruct_macros::c_extern;

c_extern! {
    pub fn bch2_make_extent_indirect(trans: *mut c::btree_trans, extent_iter: *mut c::btree_iter,
                                     orig: *mut c::bkey_i,
                                     reflink_p_may_update_opts_field: bool) -> c_int;
    pub fn bch2_indirect_extent_missing_error(trans: *mut c::btree_trans, k: c::bkey_s_c,
                                              missing_start: u64, missing_end: u64,
                                              should_commit: bool) -> c_int;
}
