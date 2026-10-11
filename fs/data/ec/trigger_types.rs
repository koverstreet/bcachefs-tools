// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of data/ec/trigger.h - C gets it as prototypes, in data/ec/trigger_gen.h,
//! which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use core::ffi::c_int;
use cstruct_macros::c_extern;

c_extern! {
    pub fn bch2_trigger_stripe(trans: *mut c::btree_trans, op: c::btree_trigger_op) -> c_int;
    pub fn bch2_stripe_handle_tryget_existing(iter: *mut c::btree_iter, s: *mut c::ec_stripe_handle) -> c_int;
    pub fn bch2_stripe_handle_put(c: *mut c::bch_fs, s: *mut c::ec_stripe_handle);
}
