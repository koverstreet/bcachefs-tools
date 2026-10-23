// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of alloc/background.h - C gets it as prototypes, in
//! alloc/background_gen.h, which is generated from this file: see
//! fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use core::ffi::{c_int, c_uint};
use cstruct_macros::{c_const, c_extern};

c_const! {
    /// How far a bucket's generation may get past its oldest pointer's before
    /// it has to be gc'd: the gen is a u8, compared wrapping.
    pub const BUCKET_GC_GEN_MAX: u32 = 96;
}

c_extern! {
    pub fn bch2_dev_allocator_set_rw(c: *mut c::bch_fs, ca: *mut c::bch_dev, rw: bool);
    pub fn bch2_recalc_capacity(c: *mut c::bch_fs);
    pub fn bch2_min_rw_member_capacity(c: *mut c::bch_fs) -> u64;
    pub fn bch2_bucket_do_freespace_index(trans: *mut c::btree_trans, ca: *mut c::bch_dev,
                                          alloc_k: c::bkey_s_c, a: *const c::AllocV4, set: bool) -> c_int;
    pub fn bch2_alloc_key_to_dev_counters(trans: *mut c::btree_trans, ca: *mut c::bch_dev,
                                          old: *const c::AllocV4, new: *const c::AllocV4,
                                          flags: c_uint) -> c_int;
}
