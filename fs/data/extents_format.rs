// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{Le16, Le32, Le64};
use cstruct_macros::{bitfield, c_bitmask, c_const, c_enum, c_verbatim, c_xmacro, CStruct, Plain,
                    TypeInfo};
use nestify::nest;

/* DOC_LATEX(extent-checksums)
 *
 * \paragraph{Why checksums are stored with keys}
 *
 * bcachefs stores checksums in the btree alongside extent pointers, not with
 * the data on disk. This is a deliberate design choice: if the checksum is
 * stored with the data, verifying it only tells you that you got \emph{a}
 * checksum that matches \emph{some} data---not that you got the data you
 * actually wanted. If a write never completed, or you're reading stale data
 * from an old location, a checksum stored with that data would still verify.
 *
 * By storing checksums in the btree, we create a chain of trust: the btree
 * says ``this extent should contain data with this checksum,'' and reading
 * the wrong data (or old data, or no data) is detected.
 *
 * \paragraph{Partial extents}
 *
 * This creates a complication: what happens when an extent is partially
 * overwritten? We can't compute a checksum for just the live portion without
 * reading the data, and the original checksum covers the original extent.
 *
 * The solution is to remember the original extent bounds. When reading a
 * trimmed extent, we read the entire original extent, verify its checksum,
 * then return only the live portion. Compression has the same constraint:
 * we can't decompress a slice, only the whole extent.
 *
 * This is handled by the \texttt{bch\_extent\_crc} structures, which store:
 * \begin{itemize}
 * \item \texttt{compressed\_size}, \texttt{uncompressed\_size}: original extent bounds
 * \item \texttt{offset}: offset into original extent where live data starts
 * \item The \texttt{size} field in the key: current live size
 * \end{itemize}
 *
 * Bucket-based allocation simplifies this: since buckets are reused all at
 * once, as long as any part of an extent is live, the rest of it remains on
 * disk. We don't need separate accounting for ``live'' vs ``needed for
 * checksum/decompression.''
 *
 * \paragraph{Per-replica formats}
 *
 * Different replicas of an extent may have different formats. When copygc or
 * tiering moves one replica, it writes only the live portion (otherwise we
 * could never reclaim the dead portions). The moved replica gets a new
 * \texttt{bch\_extent\_crc} reflecting its new bounds, while other replicas
 * keep their original format.
 *
 * To avoid storing one crc structure per pointer, extents use a compact
 * encoding: crc entries and pointers are interleaved, and each crc applies
 * to all following pointers until the next crc entry.
 */

/*
 * In extent bkeys, the value is a list of pointers (bch_extent_ptr), optionally
 * preceded by checksum/compression information (bch_extent_crc32 or
 * bch_extent_crc64).
 *
 * One major determining factor in the format of extents is how we handle and
 * represent extents that have been partially overwritten and thus trimmed:
 *
 * If an extent is not checksummed or compressed, when the extent is trimmed we
 * don't have to remember the extent we originally allocated and wrote: we can
 * merely adjust ptr->offset to point to the start of the data that is currently
 * live. The size field in struct bkey records the current (live) size of the
 * extent, and is also used to mean "size of region on disk that we point to" in
 * this case.
 *
 * Thus an extent that is not checksummed or compressed will consist only of a
 * list of bch_extent_ptrs, with none of the fields in
 * bch_extent_crc32/bch_extent_crc64.
 *
 * When an extent is checksummed or compressed, it's not possible to read only
 * the data that is currently live: we have to read the entire extent that was
 * originally written, and then return only the part of the extent that is
 * currently live.
 *
 * Thus, in addition to the current size of the extent in struct bkey, we need
 * to store the size of the originally allocated space - this is the
 * compressed_size and uncompressed_size fields in bch_extent_crc32/64. Also,
 * when the extent is trimmed, instead of modifying the offset field of the
 * pointer, we keep a second smaller offset field - "offset into the original
 * extent of the currently live region".
 *
 * The other major determining factor is replication and data migration:
 *
 * Each pointer may have its own bch_extent_crc32/64. When doing a replicated
 * write, we will initially write all the replicas in the same format, with the
 * same checksum type and compression format - however, when copygc runs later (or
 * tiering/cache promotion, anything that moves data), it is not in general
 * going to rewrite all the pointers at once - one of the replicas may be in a
 * bucket on one device that has very little fragmentation while another lives
 * in a bucket that has become heavily fragmented, and thus is being rewritten
 * sooner than the rest.
 *
 * Thus it will only move a subset of the pointers (or in the case of
 * tiering/cache promotion perhaps add a single pointer without dropping any
 * current pointers), and if the extent has been partially overwritten it must
 * write only the currently live portion (or copygc would not be able to reduce
 * fragmentation!) - which necessitates a different bch_extent_crc format for
 * the new pointer.
 *
 * But in the interests of space efficiency, we don't want to store one
 * bch_extent_crc for each pointer if we don't have to.
 *
 * Thus, a bch_extent consists of bch_extent_crc32s, bch_extent_crc64s, and
 * bch_extent_ptrs appended arbitrarily one after the other. We determine the
 * type of a given entry with a scheme similar to utf8 (except we're encoding a
 * type, not a size), encoding the type in the position of the first set bit:
 *
 * bch_extent_crc32	- 0b1
 * bch_extent_ptr	- 0b10
 * bch_extent_crc64	- 0b100
 *
 * We do it this way because bch_extent_crc32 is _very_ constrained on bits (and
 * bch_extent_crc64 is the least constrained).
 *
 * Then, each bch_extent_crc32/64 applies to the pointers that follow after it,
 * until the next bch_extent_crc32/64.
 *
 * If there are no bch_extent_crcs preceding a bch_extent_ptr, then that pointer
 * is neither checksummed nor compressed.
 */

c_xmacro! {
    BCH_EXTENT_ENTRY_TYPES(x) {
        (ptr, 0),
        (crc32, 1),
        (crc64, 2),
        (crc128, 3),
        (stripe_ptr, 4),
        (rebalance_v1, 5),
        (flags, 6),
        (reconcile, 7),
        (reconcile_bp, 8),
    }
}

c_const! {
    #[c_int]
    pub const BCH_EXTENT_ENTRY_MAX: u32 = 9;
}

c_enum! {
    #[open]
    pub enum bch_extent_entry_type: u32 {
        BCH_EXTENT_ENTRY_TYPES(f, n) => BCH_EXTENT_ENTRY_ ## f = n,
    }
}

/* Compressed/uncompressed size are stored biased by 1: */
#[bitfield(u64, ty = u32, align = 8)]
#[derive(CStruct)]
#[c_packed]
pub struct bch_extent_crc32 {
    #[bits(default = 1 << bch_extent_entry_type::BCH_EXTENT_ENTRY_crc32.0)]
    pub(self) type_:       u2,
    compressed_size_raw:   u7,
    uncompressed_size_raw: u7,
    offset:                u7,
    unused_raw:            u1,
    csum_type:             u4,
    compression_type:      u4,
    csum:                  u32,
}

c_const! {
    pub const CRC32_SIZE_MAX: u32 = 1 << 7;
    #[c_int]
    pub const CRC32_NONCE_MAX: u32 = 0;
}

nest! {
    #[repr(C, align(8))]
    #[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
    #[c_packed]
    pub struct bch_extent_crc64 {
        #[c_bitfield]
        #>[bitfield(u64)]
        pub bits: pub struct bch_extent_crc64_bits {
            #[bits(default = 1 << bch_extent_entry_type::BCH_EXTENT_ENTRY_crc64.0)]
            pub(self) type_:       u3,
            compressed_size_raw:   u9,
            uncompressed_size_raw: u9,
            offset:                u9,
            nonce:                 u10,
            csum_type:             u4,
            compression_type:      u4,
            csum_hi:               u16,
        },
        pub csum_lo: u64,
    }
}

c_const! {
    pub const CRC64_SIZE_MAX: u32 = 1 << 9;
    pub const CRC64_NONCE_MAX: u32 = (1 << 10) - 1;
}

nest! {
    #[repr(C, align(8))]
    #[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
    #[c_packed]
    pub struct bch_extent_crc128 {
        #[c_bitfield]
        #>[bitfield(u64)]
        pub bits: pub struct bch_extent_crc128_bits {
            #[bits(default = 1 << bch_extent_entry_type::BCH_EXTENT_ENTRY_crc128.0)]
            pub(self) type_:       u4,
            compressed_size_raw:   u13,
            uncompressed_size_raw: u13,
            offset:                u13,
            nonce:                 u13,
            csum_type:             u4,
            compression_type:      u4,
        },
        pub csum: c::bch_csum,
    }
}

c_const! {
    pub const CRC128_SIZE_MAX: u32 = 1 << 13;
    pub const CRC128_NONCE_MAX: u32 = (1 << 13) - 1;
}

/*
 * @reservation - pointer hasn't been written to, just reserved
 */
#[bitfield(u64, align = 8)]
#[derive(CStruct)]
#[c_packed]
pub struct bch_extent_ptr {
    #[bits(default = 1 << bch_extent_entry_type::BCH_EXTENT_ENTRY_ptr.0)]
    pub(self) type_:      u1,
    cached:     u1,
    unused:     u1,
    unwritten:  u1,
    offset:     u44, /* 8 petabytes */
    dev:        u8,
    generation: u8,
}

#[bitfield(u64)]
#[derive(CStruct)]
pub struct bch_extent_stripe_ptr {
    #[bits(default = 1 << bch_extent_entry_type::BCH_EXTENT_ENTRY_stripe_ptr.0)]
    pub(self) type_:      u5,
    block:      u8,
    redundancy: u4,
    idx:        u47,
}

c_xmacro! {
    BCH_EXTENT_FLAGS(x) {
        (poisoned, 0),
    }
}

c_enum! {
    #[open]
    pub enum bch_extent_flags_e: u32 {
        BCH_EXTENT_FLAGS(n, v) => BCH_EXTENT_FLAG_ ## n = v,
    }
}

#[bitfield(u64)]
#[derive(CStruct)]
pub struct bch_extent_flags {
    #[bits(default = 1 << bch_extent_entry_type::BCH_EXTENT_ENTRY_flags.0)]
    pub(self) type_: u7,
    flags: u57,
}

c_verbatim!(r#"
union bch_extent_entry {
	struct bch_extent_ptr ptr;
	struct bch_extent_crc32 crc32;
	struct bch_extent_crc64 crc64;
	struct bch_extent_crc128 crc128;
	struct bch_extent_stripe_ptr stripe_ptr;
	struct bch_extent_rebalance_v1 rebalance_v1;
	struct bch_extent_flags flags;
	struct bch_extent_reconcile reconcile;
	struct bch_extent_reconcile_bp reconcile_bp;
	u64 type;
};
"#);

/// An extent entry - C's union bch_extent_entry, above - as Rust names it:
/// opaque, behind a pointer or as a value's entries, since an entry is as
/// long as its arm, not the union. Rust reads entries as their arms
/// (data/extents.rs).
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, TypeInfo)]
pub struct bch_extent_entry {
    _opaque: [u64; 0],
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_btree_ptr {
    pub v: c::bch_val,

    pub _data: [u64; 0],
    pub start: [c::bch_extent_ptr; 0],
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_btree_ptr_v2 {
    pub v: c::bch_val,

    pub mem_ptr: u64,
    pub seq: Le64,
    pub sectors_written: Le16,
    pub flags: Le16,
    pub min_key: c::bpos,
    pub _data: [u64; 0],
    pub start: [c::bch_extent_ptr; 0],
}

c_bitmask! {
    LE16_BITMASK(struct bch_btree_ptr_v2, flags) {
        BTREE_PTR_RANGE_UPDATED(0, 1),
    }
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_extent {
    pub v: c::bch_val,

    pub _data: [u64; 0],
    #[c("union bch_extent_entry start[]")]
    pub start: [c::bch_extent_entry; 0],
}

c_const! {
    /* Maximum size (in u64s) a single pointer could be: */
    pub const BKEY_EXTENT_PTR_U64s_MAX: usize =
        (size_of::<c::bch_extent_crc128>() +
         size_of::<c::bch_extent_ptr>()) / size_of::<u64>();

    /*
     * Maximum possible size of an entire extent value:
     *
     * BCH_REPLICAS_MAX * 2 pointers because evacuating devices have durability=0
     * and don't count toward durability accounting, but their pointers remain
     * until the data is moved. Reconcile adds BCH_SB_MEMBER_INVALID placeholder
     * pointers to bring durability accounting up to the desired level, so both
     * sets of pointers coexist.
     */
    pub const BKEY_EXTENT_VAL_U64s_MAX: usize =
        5 + c::BKEY_EXTENT_PTR_U64s_MAX * (c::BCH_REPLICAS_MAX as usize * 2 + 1);

    /* Maximum possible size of an entire extent, key + value: */
    pub const BKEY_EXTENT_U64s_MAX: usize = c::BKEY_U64s + c::BKEY_EXTENT_VAL_U64s_MAX;

    /*
     * Btree pointers don't carry around checksums:
     *
     * BCH_REPLICAS_MAX * 2 pointers because evacuating devices have durability=0
     * and don't count toward durability accounting, but their pointers remain
     * until the data is moved. Reconcile adds BCH_SB_MEMBER_INVALID placeholder
     * pointers to bring durability accounting up to the desired level, so both
     * sets of pointers coexist.
     */
    pub const BKEY_BTREE_PTR_VAL_U64s_MAX: usize =
        (size_of::<c::bch_btree_ptr_v2>() +
         size_of::<c::bch_extent_ptr>() * c::BCH_REPLICAS_MAX as usize * 2 +
         size_of::<c::bch_extent_reconcile>() +
         size_of::<c::bch_extent_reconcile_bp>()) / size_of::<u64>();
    pub const BKEY_BTREE_PTR_U64s_MAX: usize = c::BKEY_U64s + c::BKEY_BTREE_PTR_VAL_U64s_MAX;
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_reservation {
    pub v: c::bch_val,

    pub generation: Le32,
    pub nr_replicas: u8,
    pub pad: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_inline_data {
    pub v: c::bch_val,
    pub data: [u8; 0],
}
