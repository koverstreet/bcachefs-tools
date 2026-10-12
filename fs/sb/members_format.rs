// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{c_default, Le16, Le32, Le64};
use crate::util::vstructs::vstruct;
use cstruct_macros::{c_bitmask, c_const, c_enum, c_xmacro, CStruct, Plain, TypeInfo, View};

c_const! {
    /*
     * We refer to members with bitmasks in various places - but we need to get rid
     * of this limit:
     */
    #[c_int]
    pub const BCH_SB_MEMBERS_MAX: u32 = 256;

    /*
     * Sentinal value - indicates a device that does not exist
     */
    #[c_int]
    pub const BCH_SB_MEMBER_INVALID: u32 = 255;
}

c_const! {
    #[c_int]
    pub const BCH_MIN_NR_NBUCKETS: u32 = 1 << 9;
}

c_xmacro! {
    BCH_IOPS_MEASUREMENTS(x) {
        (seqread, 0),
        (seqwrite, 1),
        (randread, 2),
        (randwrite, 3),
    }
}

c_enum! {
    #[open]
    pub enum bch_iops_measurement: u32 {
        BCH_IOPS_MEASUREMENTS(t, n) => BCH_IOPS_ ## t = n,
        BCH_IOPS_NR,
    }
}

c_xmacro! {
    BCH_MEMBER_ERROR_TYPES(x) {
        (read, 0),
        (write, 1),
        (checksum, 2),
    }
}

c_enum! {
    #[open]
    pub enum bch_member_error_type: u32 {
        BCH_MEMBER_ERROR_TYPES(t, n) => BCH_MEMBER_ERROR_ ## t = n,
        BCH_MEMBER_ERROR_NR,
    }
}

#[repr(C)]
#[derive(Clone, Copy, CStruct, Plain, TypeInfo)]
pub struct bch_member {
    pub uuid: c::__uuid_t,
    pub nbuckets: Le64, /* device size */
    pub first_bucket: Le16, /* index of first bucket used */
    pub bucket_size: Le16, /* sectors */
    pub btree_bitmap_shift: u8,
    pub pad: [u8; 3],
    pub last_mount: Le64, /* time_t */

    pub flags: Le64,
    pub iops: [Le32; 4],
    pub errors: [Le64; c::BCH_MEMBER_ERROR_NR as usize],
    pub errors_at_reset: [Le64; c::BCH_MEMBER_ERROR_NR as usize],
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

    #[c("__u8 device_name[16] __nonstring")]
    pub device_name: [u8; 16],
    #[c("__u8 device_model[64] __nonstring")]
    pub device_model: [u8; 64],
    pub flush_errors: Le64,
    #[c("__u8 device_serial[64] __nonstring")]
    pub device_serial: [u8; 64],
    /*
     * Failure domain: devices sharing a (non-empty) string are in the same
     * failure domain, and allocation spreads replicas - and, for erasure
     * coding, requires stripe blocks - across domains. A flat, intrinsic
     * device property with no relationship to the disk_groups label tree.
     * Interned to a small id in memory (bch_member_cpu.failure_domain) for
     * the allocation path.
     */
    #[c("__u8 failure_domain[32] __nonstring")]
    pub failure_domain: [u8; 32],
}
c_default!(bch_member);

c_const! {
    /*
     * btree_allocated_bitmap can represent sector addresses of a u64: it itself has
     * 64 elements, so 64 - ilog2(64)
     */
    #[c_int]
    pub const BCH_MI_BTREE_BITMAP_SHIFT_MAX: u32 = 58;

    /*
     * This limit comes from the bucket_gens array - it's a single allocation, and
     * kernel allocation are limited to INT_MAX
     */
    #[c_int]
    pub const BCH_MEMBER_NBUCKETS_MAX: u32 = c::INT_MAX as u32 - 64;

    #[c_int]
    pub const BCH_MEMBER_V1_BYTES: u32 = 56;
}

c_bitmask! {
    LE16_BITMASK(struct bch_member, bucket_size), strip BCH_MEMBER_ {
        BCH_MEMBER_BUCKET_SIZE(0, 16),
    }
}

c_bitmask! {
    LE64_BITMASK(struct bch_member, flags), strip BCH_MEMBER_ {
        BCH_MEMBER_STATE(0, 4),
        /* 4-14 unused, was TIER, HAS_(META)DATA, REPLACEMENT */
        BCH_MEMBER_DISCARD(14, 15),
        BCH_MEMBER_DATA_ALLOWED(15, 20),
        BCH_MEMBER_GROUP(20, 28),
        BCH_MEMBER_DURABILITY(28, 30),
        BCH_MEMBER_FREESPACE_INITIALIZED(30, 31),
        BCH_MEMBER_RESIZE_ON_MOUNT(31, 32),
        BCH_MEMBER_ROTATIONAL(32, 33),
        BCH_MEMBER_ROTATIONAL_SET(33, 34),
        BCH_MEMBER_INITIALIZED(34, 38),
        /* 38-46 free, was FAILURE_DOMAIN (now a string, member.failure_domain) */
    }
}

c_xmacro! {
    BCH_MEMBER_STATES(x) {
        (rw, 0),
        (ro, 1),
        (evacuating, 2),
        (spare, 3),
    }
}

c_enum! {
    #[open]
    pub enum bch_member_state: u32 {
        BCH_MEMBER_STATES(t, n) => BCH_MEMBER_STATE_ ## t = n,
        BCH_MEMBER_STATE_NR,
    }
}

c_xmacro! {
    BCH_MEMBER_INITIALIZED_STATES(x) {
        (initialized, 0),
        (pre_dev_usage, 1),
        (pre_mark_sb, 2),
        (pre_freespace_init, 3),
        (pre_journal_alloc, 4),
    }
}

c_enum! {
    #[open]
    pub enum bch_member_initialized: u32 {
        BCH_MEMBER_INITIALIZED_STATES(t, n) => BCH_MEMBER_INITIALIZED_ ## t = n,
        BCH_MEMBER_INITIALIZED_NR,
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo, View)]
pub struct bch_sb_field_members_v1 {
    pub field: c::bch_sb_field,
    pub _members: [c::bch_member; 0], //Members are now variable size
}
vstruct!(bch_sb_field_members_v1, field.u64s, start = 0);

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo, View)]
pub struct bch_sb_field_members_v2 {
    pub field: c::bch_sb_field,
    pub member_bytes: Le16, //size of single member entry
    pub pad: [u8; 6],
    pub _members: [c::bch_member; 0],
}
vstruct!(bch_sb_field_members_v2, field.u64s, start = 0);
