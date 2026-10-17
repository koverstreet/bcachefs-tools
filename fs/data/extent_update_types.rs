// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of data/extent_update.h - C gets it as prototypes, in
//! data/extent_update_gen.h, which is generated from this file: see
//! fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use core::ffi::c_int;
use cstruct_macros::c_extern;

c_extern! {
    pub fn bch2_extent_trim_atomic(trans: *mut c::btree_trans, iter: *mut c::btree_iter,
                                   k: *mut c::bkey_i) -> c_int;
}
