// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{c_default, Le64};
use core::ffi::c_ulong;
use cstruct_macros::{bitfield, xmacro, grown, whole, BitfieldMembers, MaybeZeroable, Plain, TypeInfo, View,
                     SizedVal};

#[whole(tail = data: [u8], bkey)]
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, MaybeZeroable, TypeInfo)]
pub struct Alloc {
    pub v: c::bch_val,
    pub fields: u8,
    pub generation: u8,
    pub data: [u8; 0],
}

xmacro! {
    BCH_ALLOC_FIELDS_V1 {
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

#[whole(tail = data: [u8], bkey)]
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, MaybeZeroable, TypeInfo)]
pub struct AllocV2 {
    pub v: c::bch_val,
    pub nr_fields: u8,
    pub generation: u8,
    pub oldest_gen: u8,
    pub data_type: u8,
    pub data: [u8; 0],
}

xmacro! {
    BCH_ALLOC_FIELDS_V2 {
        (read_time, 64),
        (write_time, 64),
        (dirty_sectors, 32),
        (cached_sectors, 32),
        (stripe, 32),
        (stripe_redundancy, 8),
    }
}

#[whole(tail = data: [u8], bkey)]
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo, BitfieldMembers)]
pub struct AllocV3 {
    pub v: c::bch_val,
    pub journal_seq: Le64,
    #[members]
    pub flags: AllocV3Flags,
    pub nr_fields: u8,
    pub generation: u8,
    pub oldest_gen: u8,
    pub data_type: u8,
    pub data: [u8; 0],
}

#[bitfield(Le32)]
pub struct AllocV3Flags {
    need_discard: bool,
    need_inc_gen: bool,
    __:           u30,
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
#[grown(from = journal_seq_empty, bkey)]
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo, BitfieldMembers)]
pub struct AllocV4 {
    pub v: c::bch_val,
    pub journal_seq_nonempty: u64,
    #[members]
    pub flags: AllocV4Flags,
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

#[allow(non_upper_case_globals)]
pub const BCH_ALLOC_V4_U64s_V0: u32 = 6;
#[allow(non_upper_case_globals)]
pub const BCH_ALLOC_V4_U64s: usize = size_of::<c::AllocV4>() / size_of::<u64>();

#[bitfield(u32)]
pub struct AllocV4Flags {
    need_discard:                   bool,
    need_inc_gen:                   bool,
    backpointers_start:             u6,
    nr_backpointers:                u6,
    __:                             u18,
}

#[repr(C, align(8))]
#[derive(Clone, Copy, MaybeZeroable, Plain, TypeInfo, View, SizedVal)]
pub struct BucketGens {
    pub v: c::bch_val,
    pub gens: [u8; BucketGens::NR],
}
c_default!(BucketGens);

impl BucketGens {
    /// A key's buckets: NR of them, the first at its offset << BITS
    pub const BITS: u32 = 8;
    pub const NR: usize = 1 << Self::BITS;
    pub const MASK: u64 = Self::NR as u64 - 1;
}

pub const DATA_TYPES_MOVABLE: c_ulong =
    (1 << c::DataType::btree.0) |
    (1 << c::DataType::user.0) |
    (1 << c::DataType::stripe.0) |
    (1 << c::DataType::multiple.0);
