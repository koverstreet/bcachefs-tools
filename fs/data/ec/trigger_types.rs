// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of data/ec/trigger.h - C gets it as prototypes, in data/ec/trigger_gen.h,
//! which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::c_extern;

// What Rust calls of data/ec/trigger.h: C gets these as prototypes, in data/ec/trigger_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_trigger_stripe(arg1: *mut c::btree_trans, arg2: c::btree_trigger_op) -> core::ffi::c_int;
    pub fn bch2_stripe_handle_tryget_existing(arg1: *mut c::btree_iter, arg2: *mut c::ec_stripe_handle) -> core::ffi::c_int;
    pub fn bch2_stripe_handle_put(arg1: *mut c::bch_fs, arg2: *mut c::ec_stripe_handle);
}
