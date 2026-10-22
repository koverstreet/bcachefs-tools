// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::locking::{ByteLock, Lock};
use crate::util::percpu::Counters;
use core::ffi::{c_int, c_uint};
use cstruct_macros::{CStruct, bitfield, c_const, c_extern, c_rcu_array, Fieldwise, TypeInfo};
use nestify::nest;

c_const! {
    #[c_int]
    pub const BUCKET_JOURNAL_SEQ_BITS: u32 = 16;
    #[c_int]
    pub const RESERVE_FACTOR: u32 = 6;
}

/// A bucket's gc state, in memory: fsck has one for every bucket on every
/// device, so it's 16 bytes, the lock one of them.
#[repr(C, align(4))]
#[derive(CStruct)]
pub struct bucket {
    #[c_lock("u8 lock")]
    pub lock: Lock<ByteLock, BucketData>,
}

nest! {
    /// What a bucket's lock protects. Packed, so it starts right after the
    /// lock byte; laid out so the u32s are aligned all the same.
    #[repr(C, packed)]
    #[derive(Clone, Copy, Default, CStruct)]
    pub struct BucketData {
        #[c_bitfield]
        #>[bitfield(u8)]
        pub bits: pub struct bucket_bits {
            gen_valid: u1,
            data_type: u7,
        },
        pub generation: u8,
        pub __pad: u8,
        pub dirty_sectors: u32,
        pub cached_sectors: u32,
        pub stripe_sectors: u32,
    }
}

/// The header of a device's bucket generations: C's bucket_gens fields
/// between nbuckets and b[].
#[repr(C)]
#[derive(CStruct)]
pub struct BucketGensHead {
    pub first_bucket: u16,
    pub nbuckets_minus_first: usize,
}

// SAFETY: integers; all zeroes is an empty table
unsafe impl crate::util::rcu::RcuElement for BucketGensHead {}

c_rcu_array! {
    /// ca->bucket_gens: a device's bucket generations, published under RCU.
    pub type BucketGensArray = crate::util::rcu::RcuArray<u8, c::BucketGensHead>
        as struct bucket_gens { nbuckets, b };
}

/* Only info on bucket countns: */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo, Fieldwise)]
#[fieldwise(Counters::wrapping_add(rhs), Counters::clamp_nonnegative)]
pub struct bch_dev_usage {
    pub buckets: [u64; c::BCH_DATA_NR as usize],
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo, Fieldwise)]
#[fieldwise(Counters::wrapping_add(rhs), Counters::clamp_nonnegative)]
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
#[derive(Clone, Copy, Default, CStruct, TypeInfo, Fieldwise)]
#[fieldwise(Counters::wrapping_add(rhs), Counters::clamp_nonnegative)]
pub struct bch_dev_usage_full {
    pub d: [c::bch_dev_usage_type; c::BCH_DATA_NR as usize],
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo, Fieldwise)]
#[fieldwise(Counters::wrapping_add(rhs), Counters::clamp_nonnegative)]
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
    pub fn bch2_trans_mark_dev_sb(c: *mut c::bch_fs, ca: *mut c::bch_dev,
                                  flags: c::btree_iter_update_trigger_flags) -> c_int;
    pub fn bch2_buckets_nouse_alloc(c: *mut c::bch_fs) -> c_int;
}
