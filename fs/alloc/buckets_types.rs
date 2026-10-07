// SPDX-License-Identifier: GPL-2.0

//! The data types of alloc/buckets_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{CStruct, bitfield, c_const, c_extern, c_verbatim};
use typeinfo_macros::TypeInfo;


c_const! {
    #[c_int]
    pub const BUCKET_JOURNAL_SEQ_BITS: u32 = 16;
}

c_verbatim!(r#"
/*
 * Ugly hack alert:
 *
 * We need to cram a spinlock in a single byte, because that's what we have left
 * in struct bucket, and we care about the size of these - during fsck, we need
 * in memory state for every single bucket on every device.
 *
 * We used to do
 *   while (xchg(&b->lock, 1) cpu_relax();
 * but, it turns out not all architectures support xchg on a single byte.
 *
 * So now we use bit_spin_lock(), with fun games since we can't burn a whole
 * ulong for this - we just need to make sure the lock bit always ends up in the
 * first byte.
 */
"#);

#[cfg(target_endian = "little")]
c_const! {
    #[c_int]
    pub const BUCKET_LOCK_BITNR: u32 = 0;
}

#[cfg(not(target_endian = "little"))]
c_const! {
    #[c_int]
    pub const BUCKET_LOCK_BITNR: u32 = c::BITS_PER_LONG as u32 - 1;
}

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub union ulong_byte_assert {
    #[c("ulong ulong")]
    pub ulong: c::ulong,
    pub byte: u8,
}
c_default!(ulong_byte_assert);

#[bitfield(u8)]
pub struct bucket_gen_valid_bits {
    #[bits(1)]
    pub gen_valid: u8,
    #[bits(7)]
    pub data_type: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
#[c_align("sizeof(long)")]
pub struct bucket {
    #[c_anon("")] pub __align: [core::ffi::c_long; 0],
    pub lock: u8,
    #[c_bitfield]
    pub gen_valid_bits: bucket_gen_valid_bits,
    pub generation: u8,
    pub dirty_sectors: u32,
    pub cached_sectors: u32,
    pub stripe_sectors: u32,
}
impl bucket {
    pub fn gen_valid(&self) -> u8 { let b = self.gen_valid_bits; b.gen_valid() }
    pub fn set_gen_valid(&mut self, v: u8) { let mut b = self.gen_valid_bits; b.set_gen_valid(v); self.gen_valid_bits = b; }
    pub fn data_type(&self) -> u8 { let b = self.gen_valid_bits; b.data_type() }
    pub fn set_data_type(&mut self, v: u8) { let mut b = self.gen_valid_bits; b.set_data_type(v); self.gen_valid_bits = b; }
}

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct bucket_gens {
    pub rcu: c::rcu_head,
    pub first_bucket: u16,
    pub nbuckets: usize,
    pub nbuckets_minus_first: usize,
    #[c("u8 b[] __counted_by(nbuckets)")]
    pub b: [u8; 0],
}
c_default!(bucket_gens);

/* Only info on bucket countns: */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_dev_usage {
    pub buckets: [u64; c::BCH_DATA_NR as usize],
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_dev_usage_type {
    pub buckets: u64,
    pub sectors: u64, /* _compressed_ sectors: */
    /*
     * XXX
     * Why do we have this? Isn't it just buckets * bucket_size -
     * sectors?
     */
    pub fragmented: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_dev_usage_full {
    pub d: [c::bch_dev_usage_type; c::BCH_DATA_NR as usize],
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_fs_usage_base {
    pub hidden: u64,
    pub btree: u64,
    pub data: u64,
    pub cached: u64,
    pub reserved: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_fs_usage_short {
    pub capacity: u64,
    pub used: u64,
    pub free: u64,
}

/*
 * A reservation for space on disk. @sectors is physical - total disk space, not
 * the size of the data - and is charged to the online_reserved[] slot named by
 * @nr_replicas: charged sectors always have one, and changing it means moving
 * them, with disk_res_move_slot().
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct disk_reservation {
    pub sectors: u64,
    pub generation: u32,
    pub nr_replicas: core::ffi::c_uint,
}

// What Rust calls of alloc/buckets.h: C gets these as prototypes, in alloc/buckets_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn rust_bch2_disk_reservation_add(c: *mut c::bch_fs, res: *mut c::disk_reservation, sectors: u64, nr_replicas: core::ffi::c_uint, flags: core::ffi::c_int) -> core::ffi::c_int;
    pub fn rust_bch2_disk_reservation_put(c: *mut c::bch_fs, res: *mut c::disk_reservation);
    pub fn bch2_dev_usage_full_read_fast(arg1: *mut c::bch_dev, arg2: *mut c::bch_dev_usage_full);
    pub fn bch2_fs_usage_read_short(arg1: *mut c::bch_fs) -> c::bch_fs_usage_short;
    pub fn bch2_trans_mark_dev_sb(arg1: *mut c::bch_fs, arg2: *mut c::bch_dev, arg3: c::btree_iter_update_trigger_flags) -> core::ffi::c_int;
    pub fn bch2_buckets_nouse_alloc(arg1: *mut c::bch_fs) -> core::ffi::c_int;
}
