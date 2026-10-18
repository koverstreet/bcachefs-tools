// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use core::ffi::{c_int, c_uint};
use cstruct_macros::{CStruct, c_extern, TypeInfo};
use crate::cstructs::c;

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_member_cpu {
    pub nbuckets: u64, /* device size */
    pub nbuckets_minus_first: u64,
    pub first_bucket: u16, /* index of first bucket used */
    pub bucket_size: u16, /* sectors */
    pub group: u16,
    pub failure_domain: u16,
    pub state: u8,
    pub discard: u8,
    pub data_allowed: u8,
    pub durability: u8,
    pub freespace_initialized: u8,
    pub initialized: u8,
    pub resize_on_mount: u8,
    pub rotational: u8,
    pub valid: u8,
    pub btree_bitmap_shift: u8,
    pub btree_allocated_bitmap: u64,
}

c_extern! {
    pub fn rust_bch2_dev_put(ca: *mut c::bch_dev);
    pub fn rust_bch2_dev_get(ca: *mut c::bch_dev);
    pub fn rust_bch2_dev_get_outer(ca: *mut c::bch_dev);
    pub fn rust_bch2_dev_put_outer(ca: *mut c::bch_dev);
    pub fn bch2_dev_missing_atomic(c: *mut c::bch_fs, dev: c_uint);
    pub fn bch2_dev_missing_bkey(c: *mut c::bch_fs, k: c::bkey_s_c, dev: c_uint) -> c_int;
    pub fn bch2_dev_bucket_missing(ca: *mut c::bch_dev, bucket: u64);
}
