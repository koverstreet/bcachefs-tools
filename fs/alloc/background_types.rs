// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of alloc/background.h - C gets it as prototypes, in
//! alloc/background_gen.h, which is generated from this file: see
//! fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use core::ffi::c_int;
use cstruct_macros::c_extern;

c_extern! {
    pub fn bch2_dev_allocator_set_rw(c: *mut c::bch_fs, ca: *mut c::bch_dev, rw: bool);
    pub fn bch2_recalc_capacity(c: *mut c::bch_fs);
    pub fn __bch2_alloc_to_v4(k: c::bkey_s_c, out: *mut c::AllocV4);
    pub fn bch2_alloc_to_v4_mut(trans: *mut c::btree_trans, k: c::bkey_s_c) -> *mut c::bkey_i_alloc_v4;
    pub fn bch2_bucket_do_freespace_index(trans: *mut c::btree_trans, ca: *mut c::bch_dev,
                                          alloc_k: c::bkey_s_c, a: *const c::AllocV4, set: bool) -> c_int;
}
