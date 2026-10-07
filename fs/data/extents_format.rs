// SPDX-License-Identifier: GPL-2.0

//! The data types of data/extents_format.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use zerocopy::byteorder::little_endian as le;
use cstruct_macros::{bitfield, c_bitmask, c_const, c_enum, c_xmacro, CStruct};
use typeinfo_macros::TypeInfo;

c_xmacro! {
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

macro_rules! __bch_extent_entry_type_0 {
    ([$($acc:tt)*] $(($f:tt, $n:expr)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_extent_entry_type: u32 {
                $($acc)*
                $([<BCH_EXTENT_ENTRY_ $f>] = (($n) as u32),)*
            }
        }
    } };
}
BCH_EXTENT_ENTRY_TYPES!(__bch_extent_entry_type_0 []);

#[bitfield(u64)]
pub struct bch_extent_crc32_type_bits {
    #[bits(2)]
    pub type_: u32,
    #[bits(7)]
    pub compressed_size_raw: u32,
    #[bits(7)]
    pub uncompressed_size_raw: u32,
    #[bits(7)]
    pub offset: u32,
    #[bits(1)]
    pub unused_raw: u32,
    #[bits(4)]
    pub csum_type: u32,
    #[bits(4)]
    pub compression_type: u32,
    pub csum: u32,
}

/* Compressed/uncompressed size are stored biased by 1: */
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_extent_crc32 {
    #[c_bitfield]
    pub type_bits: bch_extent_crc32_type_bits,
}
impl bch_extent_crc32 {
    pub fn type_(&self) -> u32 { let b = self.type_bits; b.type_() }
    pub fn set_type_(&mut self, v: u32) { let mut b = self.type_bits; b.set_type_(v); self.type_bits = b; }
    pub fn compressed_size_raw(&self) -> u32 { let b = self.type_bits; b.compressed_size_raw() }
    pub fn set_compressed_size_raw(&mut self, v: u32) { let mut b = self.type_bits; b.set_compressed_size_raw(v); self.type_bits = b; }
    pub fn uncompressed_size_raw(&self) -> u32 { let b = self.type_bits; b.uncompressed_size_raw() }
    pub fn set_uncompressed_size_raw(&mut self, v: u32) { let mut b = self.type_bits; b.set_uncompressed_size_raw(v); self.type_bits = b; }
    pub fn offset(&self) -> u32 { let b = self.type_bits; b.offset() }
    pub fn set_offset(&mut self, v: u32) { let mut b = self.type_bits; b.set_offset(v); self.type_bits = b; }
    pub fn unused_raw(&self) -> u32 { let b = self.type_bits; b.unused_raw() }
    pub fn set_unused_raw(&mut self, v: u32) { let mut b = self.type_bits; b.set_unused_raw(v); self.type_bits = b; }
    pub fn csum_type(&self) -> u32 { let b = self.type_bits; b.csum_type() }
    pub fn set_csum_type(&mut self, v: u32) { let mut b = self.type_bits; b.set_csum_type(v); self.type_bits = b; }
    pub fn compression_type(&self) -> u32 { let b = self.type_bits; b.compression_type() }
    pub fn set_compression_type(&mut self, v: u32) { let mut b = self.type_bits; b.set_compression_type(v); self.type_bits = b; }
    pub fn csum(&self) -> u32 { let b = self.type_bits; b.csum() }
    pub fn set_csum(&mut self, v: u32) { let mut b = self.type_bits; b.set_csum(v); self.type_bits = b; }
}

c_const! {
    pub const CRC32_SIZE_MAX: u32 = 1 << 7;
}

c_const! {
    #[c_int]
    pub const CRC32_NONCE_MAX: u32 = 0;
}

#[bitfield(u64)]
pub struct bch_extent_crc64_type_bits {
    #[bits(3)]
    pub type_: u64,
    #[bits(9)]
    pub compressed_size_raw: u64,
    #[bits(9)]
    pub uncompressed_size_raw: u64,
    #[bits(9)]
    pub offset: u64,
    #[bits(10)]
    pub nonce: u64,
    #[bits(4)]
    pub csum_type: u64,
    #[bits(4)]
    pub compression_type: u64,
    #[bits(16)]
    pub csum_hi: u64,
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_extent_crc64 {
    #[c_bitfield]
    pub type_bits: bch_extent_crc64_type_bits,
    pub csum_lo: u64,
}
impl bch_extent_crc64 {
    pub fn type_(&self) -> u64 { let b = self.type_bits; b.type_() }
    pub fn set_type_(&mut self, v: u64) { let mut b = self.type_bits; b.set_type_(v); self.type_bits = b; }
    pub fn compressed_size_raw(&self) -> u64 { let b = self.type_bits; b.compressed_size_raw() }
    pub fn set_compressed_size_raw(&mut self, v: u64) { let mut b = self.type_bits; b.set_compressed_size_raw(v); self.type_bits = b; }
    pub fn uncompressed_size_raw(&self) -> u64 { let b = self.type_bits; b.uncompressed_size_raw() }
    pub fn set_uncompressed_size_raw(&mut self, v: u64) { let mut b = self.type_bits; b.set_uncompressed_size_raw(v); self.type_bits = b; }
    pub fn offset(&self) -> u64 { let b = self.type_bits; b.offset() }
    pub fn set_offset(&mut self, v: u64) { let mut b = self.type_bits; b.set_offset(v); self.type_bits = b; }
    pub fn nonce(&self) -> u64 { let b = self.type_bits; b.nonce() }
    pub fn set_nonce(&mut self, v: u64) { let mut b = self.type_bits; b.set_nonce(v); self.type_bits = b; }
    pub fn csum_type(&self) -> u64 { let b = self.type_bits; b.csum_type() }
    pub fn set_csum_type(&mut self, v: u64) { let mut b = self.type_bits; b.set_csum_type(v); self.type_bits = b; }
    pub fn compression_type(&self) -> u64 { let b = self.type_bits; b.compression_type() }
    pub fn set_compression_type(&mut self, v: u64) { let mut b = self.type_bits; b.set_compression_type(v); self.type_bits = b; }
    pub fn csum_hi(&self) -> u64 { let b = self.type_bits; b.csum_hi() }
    pub fn set_csum_hi(&mut self, v: u64) { let mut b = self.type_bits; b.set_csum_hi(v); self.type_bits = b; }
}

c_const! {
    pub const CRC64_SIZE_MAX: u32 = 1 << 9;
}

c_const! {
    pub const CRC64_NONCE_MAX: u32 = (1 << 10) - 1;
}

#[bitfield(u64)]
pub struct bch_extent_crc128_type_bits {
    #[bits(4)]
    pub type_: u64,
    #[bits(13)]
    pub compressed_size_raw: u64,
    #[bits(13)]
    pub uncompressed_size_raw: u64,
    #[bits(13)]
    pub offset: u64,
    #[bits(13)]
    pub nonce: u64,
    #[bits(4)]
    pub csum_type: u64,
    #[bits(4)]
    pub compression_type: u64,
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_extent_crc128 {
    #[c_bitfield]
    pub type_bits: bch_extent_crc128_type_bits,
    pub csum: c::bch_csum,
}
impl bch_extent_crc128 {
    pub fn type_(&self) -> u64 { let b = self.type_bits; b.type_() }
    pub fn set_type_(&mut self, v: u64) { let mut b = self.type_bits; b.set_type_(v); self.type_bits = b; }
    pub fn compressed_size_raw(&self) -> u64 { let b = self.type_bits; b.compressed_size_raw() }
    pub fn set_compressed_size_raw(&mut self, v: u64) { let mut b = self.type_bits; b.set_compressed_size_raw(v); self.type_bits = b; }
    pub fn uncompressed_size_raw(&self) -> u64 { let b = self.type_bits; b.uncompressed_size_raw() }
    pub fn set_uncompressed_size_raw(&mut self, v: u64) { let mut b = self.type_bits; b.set_uncompressed_size_raw(v); self.type_bits = b; }
    pub fn offset(&self) -> u64 { let b = self.type_bits; b.offset() }
    pub fn set_offset(&mut self, v: u64) { let mut b = self.type_bits; b.set_offset(v); self.type_bits = b; }
    pub fn nonce(&self) -> u64 { let b = self.type_bits; b.nonce() }
    pub fn set_nonce(&mut self, v: u64) { let mut b = self.type_bits; b.set_nonce(v); self.type_bits = b; }
    pub fn csum_type(&self) -> u64 { let b = self.type_bits; b.csum_type() }
    pub fn set_csum_type(&mut self, v: u64) { let mut b = self.type_bits; b.set_csum_type(v); self.type_bits = b; }
    pub fn compression_type(&self) -> u64 { let b = self.type_bits; b.compression_type() }
    pub fn set_compression_type(&mut self, v: u64) { let mut b = self.type_bits; b.set_compression_type(v); self.type_bits = b; }
}

c_const! {
    pub const CRC128_SIZE_MAX: u32 = 1 << 13;
}

c_const! {
    pub const CRC128_NONCE_MAX: u32 = (1 << 13) - 1;
}

#[bitfield(u64)]
pub struct bch_extent_ptr_type_bits {
    #[bits(1)]
    pub type_: u64,
    #[bits(1)]
    pub cached: u64,
    #[bits(1)]
    pub unused: u64,
    #[bits(1)]
    pub unwritten: u64,
    #[bits(44)]
    pub offset: u64, /* 8 petabytes */
    #[bits(8)]
    pub dev: u64,
    #[bits(8)]
    pub generation: u64,
}

/*
 * @reservation - pointer hasn't been written to, just reserved
 */
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_extent_ptr {
    #[c_bitfield]
    pub type_bits: bch_extent_ptr_type_bits,
}
impl bch_extent_ptr {
    pub fn type_(&self) -> u64 { let b = self.type_bits; b.type_() }
    pub fn set_type_(&mut self, v: u64) { let mut b = self.type_bits; b.set_type_(v); self.type_bits = b; }
    pub fn cached(&self) -> u64 { let b = self.type_bits; b.cached() }
    pub fn set_cached(&mut self, v: u64) { let mut b = self.type_bits; b.set_cached(v); self.type_bits = b; }
    pub fn unused(&self) -> u64 { let b = self.type_bits; b.unused() }
    pub fn set_unused(&mut self, v: u64) { let mut b = self.type_bits; b.set_unused(v); self.type_bits = b; }
    pub fn unwritten(&self) -> u64 { let b = self.type_bits; b.unwritten() }
    pub fn set_unwritten(&mut self, v: u64) { let mut b = self.type_bits; b.set_unwritten(v); self.type_bits = b; }
    pub fn offset(&self) -> u64 { let b = self.type_bits; b.offset() }
    pub fn set_offset(&mut self, v: u64) { let mut b = self.type_bits; b.set_offset(v); self.type_bits = b; }
    pub fn dev(&self) -> u64 { let b = self.type_bits; b.dev() }
    pub fn set_dev(&mut self, v: u64) { let mut b = self.type_bits; b.set_dev(v); self.type_bits = b; }
    pub fn generation(&self) -> u64 { let b = self.type_bits; b.generation() }
    pub fn set_generation(&mut self, v: u64) { let mut b = self.type_bits; b.set_generation(v); self.type_bits = b; }
}

#[bitfield(u64)]
pub struct bch_extent_stripe_ptr_type_bits {
    #[bits(5)]
    pub type_: u64,
    #[bits(8)]
    pub block: u64,
    #[bits(4)]
    pub redundancy: u64,
    #[bits(47)]
    pub idx: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_extent_stripe_ptr {
    #[c_bitfield]
    pub type_bits: bch_extent_stripe_ptr_type_bits,
}
impl bch_extent_stripe_ptr {
    pub fn type_(&self) -> u64 { let b = self.type_bits; b.type_() }
    pub fn set_type_(&mut self, v: u64) { let mut b = self.type_bits; b.set_type_(v); self.type_bits = b; }
    pub fn block(&self) -> u64 { let b = self.type_bits; b.block() }
    pub fn set_block(&mut self, v: u64) { let mut b = self.type_bits; b.set_block(v); self.type_bits = b; }
    pub fn redundancy(&self) -> u64 { let b = self.type_bits; b.redundancy() }
    pub fn set_redundancy(&mut self, v: u64) { let mut b = self.type_bits; b.set_redundancy(v); self.type_bits = b; }
    pub fn idx(&self) -> u64 { let b = self.type_bits; b.idx() }
    pub fn set_idx(&mut self, v: u64) { let mut b = self.type_bits; b.set_idx(v); self.type_bits = b; }
}

c_xmacro! {
    BCH_EXTENT_FLAGS(x) {
        (poisoned, 0),
    }
}

macro_rules! __bch_extent_flags_e_0 {
    ([$($acc:tt)*] $(($n:tt, $v:expr)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_extent_flags_e: u32 {
                $($acc)*
                $([<BCH_EXTENT_FLAG_ $n>] = (($v) as u32),)*
            }
        }
    } };
}
BCH_EXTENT_FLAGS!(__bch_extent_flags_e_0 []);

#[bitfield(u64)]
pub struct bch_extent_flags_type_bits {
    #[bits(7)]
    pub type_: u64,
    #[bits(57)]
    pub flags: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_extent_flags {
    #[c_bitfield]
    pub type_bits: bch_extent_flags_type_bits,
}
impl bch_extent_flags {
    pub fn type_(&self) -> u64 { let b = self.type_bits; b.type_() }
    pub fn set_type_(&mut self, v: u64) { let mut b = self.type_bits; b.set_type_(v); self.type_bits = b; }
    pub fn flags(&self) -> u64 { let b = self.type_bits; b.flags() }
    pub fn set_flags(&mut self, v: u64) { let mut b = self.type_bits; b.set_flags(v); self.type_bits = b; }
}

/* The type word on 32-bit big-endian: C's anonymous struct, written in its
 * place by #[c_anon] - the type bits are in the second word */
#[cfg(all(not(any(target_endian = "little", target_pointer_width = "64")), target_pointer_width = "32"))]
#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_extent_entry_be32 {
    pub pad: core::ffi::c_ulong,
    pub type_: core::ffi::c_ulong,
}

macro_rules! __bch_extent_entry_0 {
    ([$($acc:tt)*] $(($f:tt$(, $($__rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        #[repr(C)]
        #[derive(Clone, Copy, CStruct, TypeInfo)]
        pub union bch_extent_entry {
            $($acc)*
            $(pub $f: c::[<bch_extent_ $f>],)*
        }
    } };
}
BCH_EXTENT_ENTRY_TYPES!(__bch_extent_entry_0 [
    #[cfg(any(target_endian = "little", target_pointer_width = "64"))]
    pub type_: core::ffi::c_ulong,
    #[cfg(all(not(any(target_endian = "little", target_pointer_width = "64")), target_pointer_width = "32"))]
    #[c_anon]
    pub be32: bch_extent_entry_be32,
]);
c_default!(bch_extent_entry);

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
    pub seq: le::U64,
    pub sectors_written: le::U16,
    pub flags: le::U16,
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
    pub start: [c::bch_extent_entry; 0],
}

c_const! {
    /* Maximum size (in u64s) a single pointer could be: */
    pub const BKEY_EXTENT_PTR_U64s_MAX: usize =
        (size_of::<c::bch_extent_crc128>() +
         size_of::<c::bch_extent_ptr>()) / size_of::<u64>();
}

c_const! {
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
}

c_const! {
    /* * Maximum possible size of an entire extent, key + value: */
    pub const BKEY_EXTENT_U64s_MAX: usize = c::BKEY_U64s + c::BKEY_EXTENT_VAL_U64s_MAX;
}

c_const! {
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
}

c_const! {
    pub const BKEY_BTREE_PTR_U64s_MAX: usize = c::BKEY_U64s + c::BKEY_BTREE_PTR_VAL_U64s_MAX;
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_reservation {
    pub v: c::bch_val,

    pub generation: le::U32,
    pub nr_replicas: u8,
    pub pad: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_inline_data {
    pub v: c::bch_val,
    pub data: [u8; 0],
}

/*
 * bch_extent_entry as a tagged_union!, over the union above. An entry is as
 * long as its arm, not the union: read entries with arm_at() over a value's
 * bytes (data/extents.rs), not get().
 */

/// The lowest set bit of the type word: extent_entry_type().
pub struct ExtentEntryType;

impl ExtentEntryType {
    /// C's `type`: the second word on 32-bit big endian.
    const WORD: usize = if cfg!(all(target_endian = "big", target_pointer_width = "32")) {
        core::mem::size_of::<core::ffi::c_ulong>()
    } else {
        0
    };

    /// The tag of the entry @b starts with - u32::MAX if its type word is 0.
    pub fn of_bytes(b: &[u8]) -> Option<u32> {
        use core::{ffi::c_ulong, mem::size_of};

        let w = c_ulong::from_ne_bytes(b.get(Self::WORD..)?.get(..size_of::<c_ulong>())?.try_into().ok()?);
        Some(if w != 0 { w.trailing_zeros() } else { u32::MAX })
    }
}

impl crate::types::Determinant<bch_extent_entry> for ExtentEntryType {
    type Tag = u32;

    fn get(u: &bch_extent_entry) -> u32 {
        Self::of_bytes(u.as_bytes()).unwrap()
    }

    /// An arm's type field is its low @tag + 1 bits, the top one set.
    fn set(u: &mut bch_extent_entry, tag: u32) {
        // SAFETY: plain data, inside the storage
        unsafe {
            let p = (u as *mut bch_extent_entry).cast::<u8>().add(Self::WORD).cast::<core::ffi::c_ulong>();
            p.write_unaligned((p.read_unaligned() & !((2 << tag) - 1)) | (1 << tag));
        }
    }
}

cstruct_macros::tagged_union! {
    pub union bch_extent_entry in bch_extent_entry {
        tag type_: u32 = c::bch_extent_entry_type by ExtentEntryType,
        arms from BCH_EXTENT_ENTRY_TYPES(f, n) => f: c::bch_extent_ ## f = n,
    }
}
