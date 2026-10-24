// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of data/ec/trigger.h - C gets it as prototypes, in
//! data/ec/trigger_gen.h, which is generated from this file: see
//! fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::fs::Fs;
use core::ffi::c_int;
use cstruct_macros::c_extern;

c_extern! {
    pub fn bch2_stripe_handle_tryget_existing(iter: *mut c::btree_iter,
                                              s: *mut c::ec_stripe_handle) -> c_int;
    pub fn bch2_stripe_handle_put(c: *mut c::bch_fs, s: *mut c::ec_stripe_handle);
    pub fn bch2_bucket_has_new_stripe(c: *mut c::bch_fs, dev_bucket: u64) -> bool;
    pub fn bch2_stripe_is_open(c: *mut c::bch_fs, idx: u64) -> bool;
}

impl Fs {
    /// Whether a stripe being made has a block in @bucket: for what would
    /// move its data meanwhile - reconcile - to leave it.
    pub fn bucket_has_new_stripe(&self, bucket: c::bpos) -> bool {
        // SAFETY: under its own lock
        unsafe { bch2_bucket_has_new_stripe(self.as_ptr(), crate::alloc::buckets::bucket_to_u64(bucket)) }
    }

    /// Whether stripe @idx is open - being made, or modified, by whoever
    /// holds its handle - and so not the stripe trigger's to delete.
    pub fn stripe_is_open(&self, idx: u64) -> bool {
        // SAFETY: under its own lock
        unsafe { bch2_stripe_is_open(self.as_ptr(), idx) }
    }
}
