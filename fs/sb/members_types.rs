// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use core::ffi::{c_int, c_uint};
use cstruct_macros::{CStruct, c_extern, TypeInfo};
use crate::cstructs::c;
use crate::util::vstructs::Head;

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
    pub fn bch2_sb_members_cpy_v2_v1(disk_sb: *mut c::bch_sb_handle) -> c_int;
    pub fn bch2_members_v2_get_mut(sb: *mut Head<c::bch_sb>, i: c_int) -> *mut c::bch_member;
    pub fn bch2_sb_member_get(sb: *mut Head<c::bch_sb>, i: c_int) -> c::bch_member;
    pub fn bch2_member_to_text(out: *mut c::printbuf, m: *mut c::bch_member,
                               gi: *mut Head<c::bch_sb_field_disk_groups>, sb: *mut Head<c::bch_sb>,
                               idx: c_uint);
    pub fn bch2_sb_nr_devices(sb: *const Head<c::bch_sb>) -> c_uint;
    pub fn rust_bch2_dev_put(ca: *mut c::bch_dev);
    pub fn rust_bch2_dev_tryget_noerror(c: *mut c::bch_fs, dev: c_uint) -> *mut c::bch_dev;
    pub fn rust_bch2_get_next_online_dev(c: *mut c::bch_fs, ca: *mut c::bch_dev, state_mask: c_uint,
                                         rw: c_int, ref_idx: c_uint) -> *mut c::bch_dev;
}
