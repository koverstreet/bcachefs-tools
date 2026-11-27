// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::c;
use crate::alloc::accounting_format::DataTypes;
use crate::format::__uuid_t;
use crate::types::{Le16, Le32, Le64};
use cstruct_macros::{bitfield, bch_enum, whole, BitfieldMembers, MaybeZeroable, Plain, TypeInfo};

/*
 * We refer to members with bitmasks in various places - but we need to get rid
 * of this limit:
 */
pub const MEMBERS_MAX: u32 = 256;

/*
 * Sentinal value - indicates a device that does not exist
 */
pub const MEMBER_INVALID: u32 = 255;

#[bch_enum(open)]
#[repr(u32)]
pub enum IopsMeasurement {
    seqread = 0,
    seqwrite = 1,
    randread = 2,
    randwrite = 3,
}

#[bch_enum(open)]
#[nr]
#[repr(u32)]
pub enum MemberErrorType {
    read = 0,
    write = 1,
    checksum = 2,
}

#[repr(C)]
#[derive(Clone, Copy, MaybeZeroable, Plain, TypeInfo, BitfieldMembers)]
pub struct Member {
    pub uuid: __uuid_t,
    pub nbuckets: Le64, /* device size */
    pub first_bucket: Le16, /* index of first bucket used */
    pub bucket_size: Le16, /* sectors */
    pub btree_bitmap_shift: u8,
    pub pad: [u8; 3],
    pub last_mount: Le64, /* time_t */

    #[members]
    pub flags: MemberFlags,
    pub iops: [Le32; 4],
    pub errors: [Le64; MemberErrorType::NR as usize],
    pub errors_at_reset: [Le64; MemberErrorType::NR as usize],
    pub errors_reset_time: Le64,
    pub seq: Le64,
    pub btree_allocated_bitmap: Le64,
    /*
     * On recovery from a clean shutdown we don't normally read the journal,
     * but we still want to resume writing from where we left off so we
     * don't overwrite more than is necessary, for list journal debugging:
     */
    pub last_journal_bucket: Le32,
    pub last_journal_bucket_offset: Le32,

    pub device_name: [u8; 16],
    pub device_model: [u8; 64],
    pub flush_errors: Le64,
    pub device_serial: [u8; 64],
    /*
     * Failure domain: devices sharing a (non-empty) string are in the same
     * failure domain, and allocation spreads replicas - and, for erasure
     * coding, requires stripe blocks - across domains. A flat, intrinsic
     * device property with no relationship to the disk_groups label tree.
     * Interned to a small id in memory (MemberCpu.failure_domain) for
     * the allocation path.
     */
    pub failure_domain: [u8; 32],
}

/*
 * btree_allocated_bitmap can represent sector addresses of a u64: it itself has
 * 64 elements, so 64 - ilog2(64)
 */
impl Member {
    pub const MIN_NBUCKETS: u32 = 1 << 9;

    pub const BTREE_BITMAP_SHIFT_MAX: u32 = 58;

    /*
     * This limit comes from the bucket_gens array - it's a single allocation, and
     * kernel allocation are limited to INT_MAX
     */
    pub const NBUCKETS_MAX: u32 = c::INT_MAX as u32 - 64;

    pub const V1_BYTES: u32 = 56;
}

#[bitfield(Le64)]
pub struct MemberFlags {
    #[bits(4)]  state:              MemberState,
    /* 4-14 unused, was TIER, HAS_(META)DATA, REPLACEMENT */
    __:                             u10,
    discard:                        bool,
    #[bits(5, from = DataTypes::from_bits_retain)]
    data_allowed:                   DataTypes,
    group:                          u8,
    durability:                     u2,
    freespace_initialized:          bool,
    resize_on_mount:                bool,
    rotational:                     bool,
    rotational_set:                 bool,
    #[bits(4)]  initialized:        MemberInitialized,
    /* 38-46 free, was FAILURE_DOMAIN (now a string, member.failure_domain) */
    __:                             u9,
    /// Reconcile parked work that can't be read without this device: the
    /// pending list is retried when it's next online
    unpark_on_online:               bool,
    __:                             u16,
}

#[bch_enum(open)]
#[nr]
#[repr(u8)]
pub enum MemberState {
    rw = 0,
    ro = 1,
    evacuating = 2,
    spare = 3,
}

#[bch_enum(open)]
#[nr]
#[repr(u8)]
pub enum MemberInitialized {
    initialized = 0,
    pre_dev_usage = 1,
    pre_mark_sb = 2,
    pre_freespace_init = 3,
    pre_journal_alloc = 4,
}

#[whole(tail = _members: [u8])]
#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo)]
pub struct SbFieldMembersV1 {
    pub _members: [Member; 0], //Members are now variable size
}

#[whole(tail = _members: [u8])]
#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo)]
pub struct SbFieldMembersV2 {
    pub member_bytes: Le16, //size of single member entry
    pub pad: [u8; 6],
    pub _members: [Member; 0],
}
