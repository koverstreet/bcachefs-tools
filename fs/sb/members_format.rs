// SPDX-License-Identifier: GPL-2.0

//! The data types of sb/members_format.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use zerocopy::byteorder::little_endian as le;
use cstruct_macros::{c_bitmask, c_const, c_enum, c_verbatim, c_xmacro, CStruct};
use typeinfo_macros::TypeInfo;

c_const! {
    /*
     * We refer to members with bitmasks in various places - but we need to get rid
     * of this limit:
     */
    #[c_int]
    pub const BCH_SB_MEMBERS_MAX: u32 = 256;
}

c_const! {
    /*
     * Sentinal value - indicates a device that does not exist
     */
    #[c_int]
    pub const BCH_SB_MEMBER_INVALID: u32 = 255;
}

c_verbatim!(r#"
#define BCH_SB_MEMBER_DELETED_UUID					\
	UUID_INIT(0xffffffff, 0xffff, 0xffff,				\
		  0xd9, 0x6a, 0x60, 0xcf, 0x80, 0x3d, 0xf7, 0xef)
"#);

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

macro_rules! __bch_iops_measurement_0 {
    ([$($acc:tt)*] $(($t:tt, $n:expr)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_iops_measurement: u32 {
                $($acc)*
                $([<BCH_IOPS_ $t>] = (($n) as u32),)*
                BCH_IOPS_NR,
            }
        }
    } };
}
BCH_IOPS_MEASUREMENTS!(__bch_iops_measurement_0 []);

c_xmacro! {
    BCH_MEMBER_ERROR_TYPES(x) {
        (read, 0),
        (write, 1),
        (checksum, 2),
    }
}

macro_rules! __bch_member_error_type_0 {
    ([$($acc:tt)*] $(($t:tt, $n:expr)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_member_error_type: u32 {
                $($acc)*
                $([<BCH_MEMBER_ERROR_ $t>] = (($n) as u32),)*
                BCH_MEMBER_ERROR_NR,
            }
        }
    } };
}
BCH_MEMBER_ERROR_TYPES!(__bch_member_error_type_0 []);

c_verbatim!(r#"
#ifndef __nonstring
#define __nonstring
#endif
"#);

#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_member {
    pub uuid: c::__uuid_t,
    pub nbuckets: le::U64, /* device size */
    pub first_bucket: le::U16, /* index of first bucket used */
    pub bucket_size: le::U16, /* sectors */
    pub btree_bitmap_shift: u8,
    pub pad: [u8; 3],
    pub last_mount: le::U64, /* time_t */

    pub flags: le::U64,
    pub iops: [le::U32; 4],
    pub errors: [le::U64; c::BCH_MEMBER_ERROR_NR as usize],
    #[c_anon("")] pub __errors_at_reset_align: [u64; 0],
    pub errors_at_reset: [le::U64; c::BCH_MEMBER_ERROR_NR as usize],
    #[c_anon("")] pub __errors_reset_time_align: [u64; 0],
    pub errors_reset_time: le::U64,
    #[c_anon("")] pub __seq_align: [u64; 0],
    pub seq: le::U64,
    #[c_anon("")] pub __btree_allocated_bitmap_align: [u64; 0],
    pub btree_allocated_bitmap: le::U64,
    /*
     * On recovery from a clean shutdown we don't normally read the journal,
     * but we still want to resume writing from where we left off so we
     * don't overwrite more than is necessary, for list journal debugging:
     */
    #[c_anon("")] pub __last_journal_bucket_align: [u32; 0],
    pub last_journal_bucket: le::U32,
    #[c_anon("")] pub __last_journal_bucket_offset_align: [u32; 0],
    pub last_journal_bucket_offset: le::U32,

    #[c("__u8 device_name[16] __nonstring")]
    pub device_name: [u8; 16],
    #[c("__u8 device_model[64] __nonstring")]
    pub device_model: [u8; 64],
    #[c_anon("")] pub __flush_errors_align: [u64; 0],
    pub flush_errors: le::U64,
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
}

c_const! {
    /*
     * This limit comes from the bucket_gens array - it's a single allocation, and
     * kernel allocation are limited to INT_MAX
     */
    #[c_int]
    pub const BCH_MEMBER_NBUCKETS_MAX: u32 = c::INT_MAX as u32 - 64;
}

c_const! {
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

c_verbatim!(r#"
#if 0
LE64_BITMASK(BCH_MEMBER_NR_READ_ERRORS,	struct bch_member, flags[1], 0,  20);
LE64_BITMASK(BCH_MEMBER_NR_WRITE_ERRORS,struct bch_member, flags[1], 20, 40);
#endif
"#);

c_xmacro! {
    BCH_MEMBER_STATES(x) {
        (rw, 0),
        (ro, 1),
        (evacuating, 2),
        (spare, 3),
    }
}

macro_rules! __bch_member_state_0 {
    ([$($acc:tt)*] $(($t:tt, $n:expr)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_member_state: u32 {
                $($acc)*
                $([<BCH_MEMBER_STATE_ $t>] = (($n) as u32),)*
                BCH_MEMBER_STATE_NR,
            }
        }
    } };
}
BCH_MEMBER_STATES!(__bch_member_state_0 []);

c_xmacro! {
    BCH_MEMBER_INITIALIZED_STATES(x) {
        (initialized, 0),
        (pre_dev_usage, 1),
        (pre_mark_sb, 2),
        (pre_freespace_init, 3),
        (pre_journal_alloc, 4),
    }
}

macro_rules! __bch_member_initialized_0 {
    ([$($acc:tt)*] $(($t:tt, $n:expr)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_member_initialized: u32 {
                $($acc)*
                $([<BCH_MEMBER_INITIALIZED_ $t>] = (($n) as u32),)*
                BCH_MEMBER_INITIALIZED_NR,
            }
        }
    } };
}
BCH_MEMBER_INITIALIZED_STATES!(__bch_member_initialized_0 []);

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_sb_field_members_v1 {
    pub field: c::bch_sb_field,
    pub _members: [c::bch_member; 0], //Members are now variable size
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_sb_field_members_v2 {
    pub field: c::bch_sb_field,
    pub member_bytes: le::U16, //size of single member entry
    pub pad: [u8; 6],
    pub _members: [c::bch_member; 0],
}
