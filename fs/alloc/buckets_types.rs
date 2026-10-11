// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{LongAligned, c_default};
use core::ffi::{c_int, c_uint};
use cstruct_macros::{CStruct, bitfield, c_const, c_extern};
use nestify::nest;
use typeinfo_macros::TypeInfo;

c_const! {
    #[c_int]
    pub const BUCKET_JOURNAL_SEQ_BITS: u32 = 16;
}

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

nest! {
    #[repr(C)]
    #[derive(Clone, Copy, Default, CStruct)]
    pub struct bucket {
        pub __align: LongAligned,
        pub lock: u8,
        #[c_bitfield]
        #>[bitfield(u8)]
        pub bits: pub struct bucket_bits {
            gen_valid: u1,
            data_type: u7,
        },
        pub generation: u8,
        pub dirty_sectors: u32,
        pub cached_sectors: u32,
        pub stripe_sectors: u32,
    }
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
    pub nr_replicas: c_uint,
}

c_extern! {
    pub fn rust_bch2_disk_reservation_add(c: *mut c::bch_fs, res: *mut c::disk_reservation,
                                          sectors: u64, nr_replicas: c_uint, flags: c_int) -> c_int;
    pub fn rust_bch2_disk_reservation_put(c: *mut c::bch_fs, res: *mut c::disk_reservation);
    pub fn bch2_dev_usage_full_read_fast(ca: *mut c::bch_dev, usage: *mut c::bch_dev_usage_full);
    pub fn bch2_fs_usage_read_short(c: *mut c::bch_fs) -> c::bch_fs_usage_short;
    pub fn bch2_trans_mark_dev_sb(c: *mut c::bch_fs, ca: *mut c::bch_dev,
                                  flags: c::btree_iter_update_trigger_flags) -> c_int;
    pub fn bch2_buckets_nouse_alloc(c: *mut c::bch_fs) -> c_int;
}
