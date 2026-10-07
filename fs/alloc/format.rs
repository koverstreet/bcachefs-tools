// SPDX-License-Identifier: GPL-2.0

//! The data types of alloc/format.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use core::ffi::c_ulong;
use zerocopy::byteorder::little_endian as le;
use cstruct_macros::{c_bitmask, c_const, c_enum, c_xmacro, CStruct};
use typeinfo_macros::TypeInfo;

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_alloc {
    pub v: c::bch_val,
    pub fields: u8,
    pub generation: u8,
    pub data: [u8; 0],
}

c_xmacro! {
    BCH_ALLOC_FIELDS_V1(x) {
        (read_time, 16),
        (write_time, 16),
        (data_type, 8),
        (dirty_sectors, 16),
        (cached_sectors, 16),
        (oldest_gen, 8),
        (stripe, 32),
        (stripe_redundancy, 8),
    }
}

macro_rules! __anon_bch_alloc_fields_v1_0 {
    ([$($acc:tt)*] $(($name:tt$(, $($__rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[closed]
            pub enum _: u32 {
                $($acc)*
                $([<BCH_ALLOC_FIELD_V1_ $name>],)*
            }
        }
    } };
}
BCH_ALLOC_FIELDS_V1!(__anon_bch_alloc_fields_v1_0 []);

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_alloc_v2 {
    pub v: c::bch_val,
    pub nr_fields: u8,
    pub generation: u8,
    pub oldest_gen: u8,
    pub data_type: u8,
    pub data: [u8; 0],
}

c_xmacro! {
    BCH_ALLOC_FIELDS_V2(x) {
        (read_time, 64),
        (write_time, 64),
        (dirty_sectors, 32),
        (cached_sectors, 32),
        (stripe, 32),
        (stripe_redundancy, 8),
    }
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_alloc_v3 {
    pub v: c::bch_val,
    pub journal_seq: le::U64,
    pub flags: le::U32,
    pub nr_fields: u8,
    pub generation: u8,
    pub oldest_gen: u8,
    pub data_type: u8,
    pub data: [u8; 0],
}

c_bitmask! {
    LE32_BITMASK(struct bch_alloc_v3, flags), strip BCH_ALLOC_V3_ {
        BCH_ALLOC_V3_NEED_DISCARD(0, 1),
        BCH_ALLOC_V3_NEED_INC_GEN(1, 2),
    }
}

/*
 * Per-bucket allocation state, stored in the alloc btree (cached).
 *
 * data_type is the bucket's state. For non-empty buckets, alloc_data_type()
 * derives it from sector counts and stripe_refcount. The empty-state
 * transitions are decided explicitly:
 *   stripe_refcount > 0	→ BCH_DATA_stripe/parity
 *   dirty_sectors > 0		→ data type from bucket contents
 *   cached_sectors > 0		→ BCH_DATA_cached
 *   nonempty → empty		→ BCH_DATA_need_discard (set in bch2_trigger_alloc)
 *   need_discard → empty	→ BCH_DATA_free (set in bch2_discard_one_bucket)
 *   gc_gen >= MAX, free	→ BCH_DATA_need_gc_gens (in alloc_data_type)
 *
 * NEED_DISCARD flag is no longer read by alloc_data_type(); it's still
 * written for forward/back compatibility with kernels that do read it.
 *
 * journal_seq_nonempty/journal_seq_empty track bucket state transitions for
 * the noflush optimization and discard path:
 *   journal_seq_nonempty: set on empty→nonempty transition
 *   journal_seq_empty:    set on nonempty→empty transition;
 *     bucket can't be reused until this seq is flushed to disk.
 *     0 means no journal delay needed (noflush/fast discard path).
 */
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_alloc_v4 {
    pub v: c::bch_val,
    pub journal_seq_nonempty: u64,
    pub flags: u32,
    pub generation: u8,
    pub oldest_gen: u8,
    pub data_type: u8,
    pub stripe_redundancy_obsolete: u8,
    pub dirty_sectors: u32,
    pub cached_sectors: u32,
    pub io_time: [u64; 2],
    pub stripe_refcount: u32,
    pub nr_external_backpointers: u32,
    /* end of fields in original version of alloc_v4 */
    pub journal_seq_empty: u64,
    pub stripe_sectors: u32,
    pub pad: u32,
}

c_const! {
    #[c_int]
    pub const BCH_ALLOC_V4_U64s_V0: u32 = 6;
}

c_const! {
    pub const BCH_ALLOC_V4_U64s: usize = size_of::<c::bch_alloc_v4>() / size_of::<u64>();
}

c_bitmask! {
    BITMASK(struct bch_alloc_v4, flags), strip BCH_ALLOC_V4_ {
        BCH_ALLOC_V4_NEED_DISCARD(0, 1),
        BCH_ALLOC_V4_NEED_INC_GEN(1, 2),
        BCH_ALLOC_V4_BACKPOINTERS_START(2, 8),
        BCH_ALLOC_V4_NR_BACKPOINTERS(8, 14),
    }
}

c_const! {
    #[c_int]
    pub const KEY_TYPE_BUCKET_GENS_BITS: u32 = 8;
}

c_const! {
    pub const KEY_TYPE_BUCKET_GENS_NR: u32 = 1 << c::KEY_TYPE_BUCKET_GENS_BITS;
}

c_const! {
    pub const KEY_TYPE_BUCKET_GENS_MASK: u32 = c::KEY_TYPE_BUCKET_GENS_NR - 1;
}

#[repr(C, align(8))]
#[derive(Clone, Copy, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_bucket_gens {
    pub v: c::bch_val,
    pub gens: [u8; c::KEY_TYPE_BUCKET_GENS_NR as usize],
}
c_default!(bch_bucket_gens);

c_const! {
    pub const DATA_TYPES_MOVABLE: c_ulong =
        (1 << c::BCH_DATA_btree as u32) |
        (1 << c::BCH_DATA_user as u32) |
        (1 << c::BCH_DATA_stripe as u32) |
        (1 << c::BCH_DATA_multiple as u32);
}
