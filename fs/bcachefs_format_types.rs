// SPDX-License-Identifier: GPL-2.0

//! The data types of bcachefs_format_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use zerocopy::byteorder::little_endian as le;
use cstruct_macros::{bitfield, c_bitmask, c_const, c_enum, c_typedef, c_verbatim, c_xmacro, CStruct};
use nestify::nest;
use typeinfo_macros::TypeInfo;

c_verbatim!(r#"
#include "enum_kind.h"

/*
 * bcachefs on disk data structures
 *
 * OVERVIEW:
 *
 * There are three main types of on disk data structures in bcachefs (this is
 * reduced from 5 in bcache)
 *
 *  - superblock
 *  - journal
 *  - btree
 *
 * The btree is the primary structure; most metadata exists as keys in the
 * various btrees. There are only a small number of btrees, they're not
 * sharded - we have one btree for extents, another for inodes, et cetera.
 *
 * SUPERBLOCK:
 *
 * The superblock contains the location of the journal, the list of devices in
 * the filesystem, and in general any metadata we need in order to decide
 * whether we can start a filesystem or prior to reading the journal/btree
 * roots.
 *
 * The superblock is extensible, and most of the contents of the superblock are
 * in variable length, type tagged fields; see struct bch_sb_field.
 *
 * Backup superblocks do not reside in a fixed location; also, superblocks do
 * not have a fixed size. To locate backup superblocks we have struct
 * bch_sb_layout; we store a copy of this inside every superblock, and also
 * before the first superblock.
 *
 * JOURNAL:
 *
 * The journal primarily records btree updates in the order they occurred;
 * journal replay consists of just iterating over all the keys in the open
 * journal entries and re-inserting them into the btrees.
 *
 * The journal also contains entry types for the btree roots, and blacklisted
 * journal sequence numbers (see journal_seq_blacklist.c).
 *
 * BTREE:
 *
 * bcachefs btrees are copy on write b+ trees, where nodes are big (typically
 * 128k-256k) and log structured. We use struct btree_node for writing the first
 * entry in a given node (offset 0), and struct btree_node_entry for all
 * subsequent writes.
 *
 * After the header, btree node entries contain a list of keys in sorted order.
 * Values are stored inline with the keys; since values are variable length (and
 * keys effectively are variable length too, due to packing) we can't do random
 * access without building up additional in memory tables in the btree node read
 * path.
 *
 * BTREE KEYS (struct bkey):
 *
 * The various btrees share a common format for the key - so as to avoid
 * switching in fastpath lookup/comparison code - but define their own
 * structures for the key values.
 *
 * The size of a key/value pair is stored as a u8 in units of u64s, so the max
 * size is just under 2k. The common part also contains a type tag for the
 * value, and a format field indicating whether the key is packed or not (and
 * also meant to allow adding new key fields in the future, if desired).
 *
 * bkeys, when stored within a btree node, may also be packed. In that case, the
 * bkey_format in that node is used to unpack it. Packed bkeys mean that we can
 * be generous with field sizes in the common part of the key format (64 bit
 * inode number, 64 bit offset, 96 bit version field, etc.) for negligible cost.
 */

#include <asm/types.h>
#include <asm/byteorder.h>
#include <linux/kernel.h>
#include <linux/uuid.h>
#include <uapi/linux/magic.h>

#include "util/vstructs.h"
"#);

#[cfg(__KERNEL__)]
c_typedef! {
    pub type __uuid_t = c::uuid_t;
}

c_verbatim!(r#"
#define BITMASK(name, type, field, offset, end)				\
static const __maybe_unused unsigned	name##_OFFSET = offset;		\
static const __maybe_unused unsigned	name##_BITS = (end - offset);	\
									\
static inline __u64 name(const type *k)					\
{									\
	return (k->field >> offset) & ~(~0ULL << (end - offset));	\
}									\
									\
static inline void SET_##name(type *k, __u64 v)				\
{									\
	k->field &= ~(~(~0ULL << (end - offset)) << offset);		\
	k->field |= (v & ~(~0ULL << (end - offset))) << offset;		\
}

#define LE_BITMASK(_bits, name, type, field, offset, end)		\
static const __maybe_unused unsigned	name##_OFFSET = offset;		\
static const __maybe_unused unsigned	name##_BITS = (end - offset);	\
static const __maybe_unused __u##_bits	name##_MAX = (1ULL << (end - offset)) - 1;\
									\
static inline __u64 name(const type *k)					\
{									\
	return (__le##_bits##_to_cpu(k->field) >> offset) &		\
		~(~0ULL << (end - offset));				\
}									\
									\
static inline void SET_##name(type *k, __u64 v)				\
{									\
	__u##_bits new = __le##_bits##_to_cpu(k->field);		\
									\
	new &= ~(~(~0ULL << (end - offset)) << offset);			\
	new |= (v & ~(~0ULL << (end - offset))) << offset;		\
	k->field = __cpu_to_le##_bits(new);				\
}

#define LE16_BITMASK(n, t, f, o, e)	LE_BITMASK(16, n, t, f, o, e)
#define LE32_BITMASK(n, t, f, o, e)	LE_BITMASK(32, n, t, f, o, e)
#define LE64_BITMASK(n, t, f, o, e)	LE_BITMASK(64, n, t, f, o, e)
"#);

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct)]
pub struct bkey_format {
    pub key_u64s: u8,
    pub nr_fields: u8,
    /* One unused slot for now: */
    pub bits_per_field: [u8; 6],
    pub field_offset: [le::U64; 6],
}

/* Btree keys - all units are in sectors */

#[cfg_attr(target_endian = "little", repr(C, packed(4)))]
#[cfg_attr(not(target_endian = "little"), repr(C, packed))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bpos {
    /*
     * Word order matches machine byte order - btree code treats a bpos as a
     * single large integer, for search/comparison purposes
     *
     * Note that wherever a bpos is embedded in another on disk data
     * structure, it has to be byte swabbed when reading in metadata that
     * wasn't written in native endian order:
     */
    #[cfg(target_endian = "little")]
    pub snapshot: u32,
    #[cfg(target_endian = "little")]
    pub offset: u64,
    #[cfg(target_endian = "little")]
    pub inode: u64,
    #[cfg(target_endian = "big")]
    pub inode: u64,
    #[cfg(target_endian = "big")]
    pub offset: u64, /* Points to end of extent - sectors */
    #[cfg(target_endian = "big")]
    pub snapshot: u32,
}

/* Empty placeholder struct, for container_of() */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_val {
    pub __nothing: [u64; 0],
}

#[cfg_attr(target_endian = "little", repr(C, packed(4)))]
#[cfg_attr(not(target_endian = "little"), repr(C, packed))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bversion {
    #[cfg(target_endian = "little")]
    pub lo: u64,
    #[cfg(target_endian = "little")]
    pub hi: u32,
    #[cfg(target_endian = "big")]
    pub hi: u32,
    #[cfg(target_endian = "big")]
    pub lo: u64,
}

#[bitfield(u8)]
pub struct bkey_format_bits {
    #[bits(7)]
    pub format: u8,
    #[bits(1)]
    pub needs_whiteout: u8,
}

/*
 * The big-endian version of bkey can't be compiled by rustc with the "aligned"
 * attr since it doesn't allow types to have both "packed" and "aligned" attrs.
 * So for Rust compatibility, don't include this. It can be included in the LE
 * version because the "packed" attr is redundant in that case.
 *
 * History: (quoting Kent)
 *
 * Specifically, when i was designing bkey, I wanted the header to be no
 * bigger than necessary so that bkey_packed could use the rest. That means that
 * decently offten extent keys will fit into only 8 bytes, instead of spilling over
 * to 16.
 *
 * But packed_bkey treats the part after the header - the packed section -
 * as a single multi word, variable length integer. And bkey, the unpacked
 * version, is just a special case version of a bkey_packed; all the packed
 * bkey code will work on keys in any packed format, the in-memory
 * representation of an unpacked key also is just one type of packed key...
 *
 * So that constrains the key part of a bkig endian bkey to start right
 * after the header.
 *
 * If we ever do a bkey_v2 and need to expand the hedaer by another byte for
 * some reason - that will clean up this wart.
 */
#[cfg_attr(target_endian = "little", repr(C, align(8)))]
#[cfg_attr(not(target_endian = "little"), repr(C, packed))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bkey {
    /* Size of combined key and value, in u64s */
    pub u64s: u8,

    /* Format of key (0 for format local to btree node) */
    #[c_bitfield]
    pub format_bits: bkey_format_bits,

    /* Type of the value */
    pub type_: u8,

    #[cfg(target_endian = "little")]
    pub pad: [u8; 1],

    #[cfg(target_endian = "little")]
    pub bversion: c::bversion,
    #[cfg(target_endian = "little")]
    pub size: u32, /* extent size, in sectors */
    #[cfg(target_endian = "little")]
    pub p: c::bpos,
    #[cfg(target_endian = "big")]
    pub p: c::bpos,
    #[cfg(target_endian = "big")]
    pub size: u32, /* extent size, in sectors */
    #[cfg(target_endian = "big")]
    pub bversion: c::bversion,

    #[cfg(target_endian = "big")]
    pub pad: [u8; 1],
}
impl bkey {
    pub fn format(&self) -> u8 { let b = self.format_bits; b.format() }
    pub fn set_format(&mut self, v: u8) { let mut b = self.format_bits; b.set_format(v); self.format_bits = b; }
    pub fn needs_whiteout(&self) -> u8 { let b = self.format_bits; b.needs_whiteout() }
    pub fn set_needs_whiteout(&mut self, v: u8) { let mut b = self.format_bits; b.set_needs_whiteout(v); self.format_bits = b; }
}

#[bitfield(u8)]
pub struct bkey_packed_format_bits {
    #[bits(7)]
    pub format: u8,
    #[bits(1)]
    pub needs_whiteout: u8,
}

#[repr(C, align(8))]
#[derive(Clone, Copy, CStruct)]
#[c_packed]
pub struct bkey_packed {
    pub _data: [u64; 0],

    /* Size of combined key and value, in u64s */
    pub u64s: u8,

    /* Format of key (0 for format local to btree node) */

    /*
     * XXX: next incompat on disk format change, switch format and
     * needs_whiteout - bkey_packed() will be cheaper if format is the high
     * bits of the bitfield
     */
    #[c_bitfield]
    pub format_bits: bkey_packed_format_bits,

    /* Type of the value */
    pub type_: u8,
    pub key_start: [u8; 0],

    /*
     * We copy bkeys with struct assignment in various places, and while
     * that shouldn't be done with packed bkeys we can't disallow it in C,
     * and it's legal to cast a bkey to a bkey_packed  - so padding it out
     * to the same size as struct bkey should hopefully be safest.
     */
    #[c("__u8 pad[sizeof(struct bkey) - 3]")]
    pub pad: [u8; core::mem::size_of::<c::bkey>() - 3],
}
c_default!(bkey_packed);
impl bkey_packed {
    pub fn format(&self) -> u8 { let b = self.format_bits; b.format() }
    pub fn set_format(&mut self, v: u8) { let mut b = self.format_bits; b.set_format(v); self.format_bits = b; }
    pub fn needs_whiteout(&self) -> u8 { let b = self.format_bits; b.needs_whiteout() }
    pub fn set_needs_whiteout(&mut self, v: u8) { let mut b = self.format_bits; b.set_needs_whiteout(v); self.format_bits = b; }
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_typedef]
pub struct bch_le128 {
    pub lo: le::U64,
    pub hi: le::U64,
}

c_const! {
    pub const BKEY_U64s: usize = size_of::<c::bkey>() / size_of::<u64>();
}

c_const! {
    pub const BKEY_U64s_MAX: u8 = u8::MAX;
}

c_const! {
    pub const BKEY_VAL_U64s_MAX: usize = c::BKEY_U64s_MAX as usize - c::BKEY_U64s;
}

c_const! {
    #[c_int]
    pub const KEY_PACKED_BITS_START: u32 = 24;
}

c_const! {
    #[c_int]
    pub const KEY_FORMAT_LOCAL_BTREE: u32 = 0;
}

c_const! {
    #[c_int]
    pub const KEY_FORMAT_CURRENT: u32 = 1;
}

c_enum! {
    #[closed]
    pub enum bch_bkey_fields: u32 {
        BKEY_FIELD_INODE,
        BKEY_FIELD_OFFSET,
        BKEY_FIELD_SNAPSHOT,
        BKEY_FIELD_SIZE,
        BKEY_FIELD_VERSION_HI,
        BKEY_FIELD_VERSION_LO,
        BKEY_NR_FIELDS,
    }
}

c_verbatim!(r#"
#define bkey_format_field(name, field)					\
	[BKEY_FIELD_##name] = (sizeof(((struct bkey *) NULL)->field) * 8)

#define BKEY_FORMAT_CURRENT						\
((struct bkey_format) {							\
	.key_u64s	= BKEY_U64s,					\
	.nr_fields	= BKEY_NR_FIELDS,				\
	.bits_per_field = {						\
		bkey_format_field(INODE,	p.inode),		\
		bkey_format_field(OFFSET,	p.offset),		\
		bkey_format_field(SNAPSHOT,	p.snapshot),		\
		bkey_format_field(SIZE,		size),			\
		bkey_format_field(VERSION_HI,	bversion.hi),		\
		bkey_format_field(VERSION_LO,	bversion.lo),		\
	},								\
})
"#);

/* bkey with inline value */
#[repr(C)]
#[derive(Default, CStruct)]
pub struct bkey_i {
    pub _data: [u64; 0],

    pub k: c::bkey,
    pub v: c::bch_val,
}

c_verbatim!(r#"
#define POS_KEY(_pos)							\
((struct bkey) {							\
	.u64s		= BKEY_U64s,					\
	.format		= KEY_FORMAT_CURRENT,				\
	.p		= _pos,						\
})

#define KEY(_inode, _offset, _size)					\
((struct bkey) {							\
	.u64s		= BKEY_U64s,					\
	.format		= KEY_FORMAT_CURRENT,				\
	.p		= POS(_inode, _offset),				\
	.size		= _size,					\
})

#define bkey_bytes(_k)		((_k)->u64s * sizeof(__u64))

#define __BKEY_PADDED(key, pad)					\
	struct bkey_i key; __u64 key ## _pad[pad]
"#);

c_enum! {
    #[flags]
    pub enum bch_bkey_type_flags: u32 {
        BKEY_TYPE_strict_btree_checks = 1 << 0,
    }
}

c_xmacro! {
    /*
     * - DELETED keys are used internally to mark keys that should be ignored but
     *   override keys in composition order.  Their version number is ignored.
     *
     * - DISCARDED keys indicate that the data is all 0s because it has been
     *   discarded. DISCARDs may have a version; if the version is nonzero the key
     *   will be persistent, otherwise the key will be dropped whenever the btree
     *   node is rewritten (like DELETED keys).
     *
     * - ERROR: any read of the data returns a read error, as the data was lost due
     *   to a failing device. Like DISCARDED keys, they can be removed (overridden)
     *   by new writes or cluster-wide GC. Node repair can also overwrite them with
     *   the same or a more recent version number, but not with an older version
     *   number.
     *
     * - WHITEOUT: for hash table btrees
     */
    BCH_BKEY_TYPES(x) {
        (deleted,               0,  0,
         "Transient during btree updates; inserting a deleted key "
         "removes any existing key at that position. "
         "Stripped during btree node writes."),
        (whiteout,              1,  0,
         "Blocks visibility of ancestor snapshot versions of a key"),
        (error,                 2,  0,
         "Marks an extent as containing unrecoverable errors"),
        (cookie,                3,  0,
         "Used by reconcile to track option changes via "
         "incrementing version numbers in the reconcile_scan btree"),
        (hash_whiteout,         4,  BKEY_TYPE_strict_btree_checks,
         "Whiteout for hash table btrees (dirents, xattrs) that "
         "preserves hash chain integrity"),
        (btree_ptr,             5,  BKEY_TYPE_strict_btree_checks,
         "Btree node pointer (v1, legacy)"),
        (extent,                6,  BKEY_TYPE_strict_btree_checks,
         "File data extent with device pointers, checksums, and "
         "optional compression metadata"),
        (reservation,           7,  BKEY_TYPE_strict_btree_checks,
         "Disk space reservation for a file region"),
        (inode,                 8,  BKEY_TYPE_strict_btree_checks,
         "Inode metadata (v1, legacy)"),
        (inode_generation,      9,  BKEY_TYPE_strict_btree_checks,
         "Tracks generation numbers for deleted inodes"),
        (dirent,                10, BKEY_TYPE_strict_btree_checks,
         "Directory entry with name hash, inode number, and type"),
        (xattr,                 11, BKEY_TYPE_strict_btree_checks,
         "Extended attribute with name hash and value"),
        (alloc,                 12, BKEY_TYPE_strict_btree_checks,
         "Bucket allocation metadata (v1, legacy)"),
        (quota,                 13, BKEY_TYPE_strict_btree_checks,
         "Quota counters for a user, group, or project"),
        (stripe,                14, BKEY_TYPE_strict_btree_checks,
         "Erasure code stripe metadata with parity pointers"),
        (reflink_p,             15, BKEY_TYPE_strict_btree_checks,
         "Pointer from the extents btree to an indirect extent "
         "in the reflink btree"),
        (reflink_v,             16, BKEY_TYPE_strict_btree_checks,
         "Indirect extent with refcount in the reflink btree"),
        (inline_data,           17, BKEY_TYPE_strict_btree_checks,
         "Small file data stored inline in the btree"),
        (btree_ptr_v2,          18, BKEY_TYPE_strict_btree_checks,
         "Btree node pointer (v2) with sequence number and "
         "min_key for more efficient traversal"),
        (indirect_inline_data,  19, BKEY_TYPE_strict_btree_checks,
         "Reflinked inline data with refcount"),
        (alloc_v2,              20, BKEY_TYPE_strict_btree_checks,
         "Bucket allocation metadata (v2, legacy)"),
        (subvolume,             21, BKEY_TYPE_strict_btree_checks,
         "Subvolume metadata with root inode and snapshot ID"),
        (snapshot,              22, BKEY_TYPE_strict_btree_checks,
         "Snapshot tree node with parent, children, and "
         "subvolume references"),
        (inode_v2,              23, BKEY_TYPE_strict_btree_checks,
         "Inode metadata (v2) with journal sequence number"),
        (alloc_v3,              24, BKEY_TYPE_strict_btree_checks,
         "Bucket allocation metadata (v3, legacy)"),
        (set,                   25, 0,
         "Empty value; presence of the key is the information"),
        (lru,                   26, BKEY_TYPE_strict_btree_checks,
         "LRU tracking entry for cache eviction"),
        (alloc_v4,              27, BKEY_TYPE_strict_btree_checks,
         "Bucket allocation metadata (v4, current)"),
        (backpointer,           28, BKEY_TYPE_strict_btree_checks,
         "Reverse pointer from a bucket back to the extent "
         "referencing it"),
        (inode_v3,              29, BKEY_TYPE_strict_btree_checks,
         "Inode metadata (v3, current) with compact encoding"),
        (bucket_gens,           30, BKEY_TYPE_strict_btree_checks,
         "Packed bucket generation numbers (256 per key)"),
        (snapshot_tree,         31, BKEY_TYPE_strict_btree_checks,
         "Root entry for a snapshot tree structure"),
        (logged_op_truncate,    32, BKEY_TYPE_strict_btree_checks,
         "Logged truncate operation for crash recovery"),
        (logged_op_finsert,     33, BKEY_TYPE_strict_btree_checks,
         "Logged file insert/collapse operation for crash recovery"),
        (accounting,            34, BKEY_TYPE_strict_btree_checks,
         "Disk accounting delta "
         "(replicas, compression, device usage)"),
        (inode_alloc_cursor,    35, BKEY_TYPE_strict_btree_checks,
         "Per-CPU inode number allocation cursor"),
        (extent_whiteout,       36, BKEY_TYPE_strict_btree_checks,
         "Whiteout specific to the extents btree, blocking "
         "visibility of ancestor snapshot extent versions"),
        (logged_op_stripe_update, 37, BKEY_TYPE_strict_btree_checks,
         "Logged stripe creation/update operation for crash recovery"),
        (damage,                38, BKEY_TYPE_strict_btree_checks,
         "Errors that damaged an inode, recorded in the same "
         "transaction as the repair that did the damage: a sorted "
         "list of bch_sb_error_id"),
        (logged_op_inode_opt_propagate, 39, BKEY_TYPE_strict_btree_checks,
         "Logged propagation of inode io options to ancestor "
         "snapshot versions"),
    }
}

macro_rules! __bch_bkey_type_0 {
    ([$($acc:tt)*] $(($name:tt, $nr:expr$(, $($__rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_bkey_type: u32 {
                $($acc)*
                $([<KEY_TYPE_ $name>] = (($nr) as u32),)*
                KEY_TYPE_MAX,
            }
        }
    } };
}
BCH_BKEY_TYPES!(__bch_bkey_type_0 []);

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_deleted {
    pub v: c::bch_val,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_whiteout {
    pub v: c::bch_val,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_extent_whiteout {
    pub v: c::bch_val,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_error {
    pub v: c::bch_val,
    pub err: u8,
    pub pad: [u8; 7],
}

c_xmacro! {
    /*
     * Why an extent became a KEY_TYPE_error key, recorded in the tombstone so a
     * later reader can tell what killed the data without the damage btree, which
     * fsck can prune.
     *
     * The decompress_ reasons are the data_decompress_err_ list in
     * sb/errors_format.h, name for name: the superblock counts how often a
     * decompression failed each way, and the key says which of those killed this
     * extent. bch2_decompress_key_type_error() maps between them off the same
     * table that produces the sb error ids, so the two can't drift - adding a
     * decompress error id without a reason here breaks the build.
     */
    KEY_TYPE_ERRORS(x) {
        (unknown, 0),
        (device_removed, 1),
        (double_allocation, 2),
        (no_valid_pointers_repair, 3),
        (decompress_exceeded_max_encoded_extent, 4),
        (decompress_lz4_old, 5),
        (decompress_lz4, 6),
        (decompress_gzip, 7),
        (decompress_gzip_size_mismatch, 8),
        (decompress_zstd_src_len_bad, 9),
        (decompress_zstd_size_mismatch, 10),
        (decompress_zstd_corruption_detected, 11),
        (decompress_zstd_checksum_wrong, 12),
        (decompress_zstd_prefix_unknown, 13),
        (decompress_zstd_src_size_wrong, 14),
        (decompress_zstd_dst_size_too_small, 15),
        (decompress_zstd_memory_allocation, 16),
        (decompress_zstd_unknown, 17),
        (decompress_unknown, 18),
    }
}

macro_rules! __bch_key_type_errors_0 {
    ([$($acc:tt)*] $(($n:tt, $t:expr)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_key_type_errors: u32 {
                $($acc)*
                $([<KEY_TYPE_ERROR_ $n>] = (($t) as u32),)*
            }
        }
    } };
}
KEY_TYPE_ERRORS!(__bch_key_type_errors_0 []);

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_cookie {
    pub v: c::bch_val,
    pub cookie: le::U64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_hash_whiteout {
    pub v: c::bch_val,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_set {
    pub v: c::bch_val,
}

/* 128 bits, sufficient for cryptographic MACs: */
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_csum {
    pub lo: le::U64,
    pub hi: le::U64,
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_backpointer {
    pub v: c::bch_val,
    pub btree_id: u8,
    pub level: u8,
    pub data_type: u8,
    pub bucket_gen: u8,
    pub flags: u32,
    pub bucket_len: u32,
    pub pos: c::bpos,
}

c_bitmask! {
    BITMASK(struct bch_backpointer, flags), strip BACKPOINTER_ {
        /*
         * Denormalized cache of "is this bp's extent listed in reconcile_phys?":
         *
         * fsck needs to check that the reconcile_phys btrees agree with the extents
         * btree, but a direct reconcile_phys <-> extents check would be expensive:
         * for every reconcile_phys entry (keyed by bucket position), we'd have to do
         * a random lookup against the extents btree. Instead, mirror the work_id into
         * the backpointer so fsck can do two cheap pairwise checks:
         *
         *   - reconcile_phys <-> backpointers: both keyed by bucket position, indexed
         *   - backpointers   <-> extents:	already required for backpointer fsck
         *
         * Invariant: when bp.flags PHYS != 0, reconcile_phys[PHYS] must have an entry
         * at bp.k.p, and the extent must carry a bch_extent_reconcile entry whose
         * rb_work_id_phys() equals PHYS. Every path that mutates reconcile_opts on an
         * extent must keep the bp's PHYS flag in sync.
         */
        BACKPOINTER_RECONCILE_PHYS(0, 2),
        BACKPOINTER_ERASURE_CODED(2, 3),
        BACKPOINTER_STRIPE_PTR(3, 4),
    }
}

/* Optional/variable size superblock sections: */

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_sb_field {
    pub _data: [u64; 0],
    pub u64s: le::U32,
    pub type_: le::U32,
}

c_xmacro! {
    BCH_SB_FIELDS(x) {
        (journal,               0,
         "Journal bucket list for this device"),
        (members_v1,            1,
         "Member device list (v1)"),
        (crypt,                 2,
         "Encryption key and KDF settings"),
        (replicas_v0,           3,
         "Replica entries (v0)"),
        (quota,                 4,
         "Quota timelimit and warnlimit fields"),
        (disk_groups,           5,
         "Device label strings and label path tree structure"),
        (clean,                 6,
         "Clean shutdown: btree roots and usage counters, "
         "allowing journal replay to be skipped"),
        (replicas,              7,
         "Replica entries: lists of devices with "
         "extents replicated across them"),
        (journal_seq_blacklist, 8,
         "Blacklisted journal sequence numbers"),
        (journal_v2,            9,
         "Journal bucket list (v2)"),
        (counters,              10,
         "Persistent counters: IO statistics, error counts, "
         "and cumulative metrics across mounts"),
        (members_v2,            11,
         "Member device list (v2)"),
        (errors,                12,
         "Persistent error log: records errors detected "
         "during operation or fsck so they survive "
         "across mounts"),
        (ext,                   13,
         "Extended superblock: required recovery passes, "
         "accumulated silenced errors, and other flags"),
        (downgrade,             14,
         "Minimum on-disk format version for safe "
         "downgrade, with required recovery passes"),
        (recovery_passes,       15,
         "Tracks which recovery passes have been run "
         "successfully"),
        (extent_type_u64s,      16,
         "Per-extent-type size limits"),
        (errors_v2,             17,
         "Persistent error log, v2: adds the time of "
         "first occurrence to each entry"),
    }
}

c_enum! {
    #[flags]
    pub enum btree_id_flags: u32 {
        BTREE_IS_extents = 1 << 0,
        BTREE_IS_snapshots = 1 << 1,
        BTREE_IS_snapshot_field = 1 << 2,
        BTREE_IS_data = 1 << 3,
        BTREE_IS_write_buffer = 1 << 4,
    }
}

c_xmacro! {
    BCH_BTREE_IDS(x) {
        (extents,               0,
         BTREE_IS_extents |
         BTREE_IS_snapshots |
         BTREE_IS_data,
         BIT_ULL(KEY_TYPE_whiteout) |
         BIT_ULL(KEY_TYPE_extent_whiteout) |
         BIT_ULL(KEY_TYPE_error) |
         BIT_ULL(KEY_TYPE_cookie) |
         BIT_ULL(KEY_TYPE_extent) |
         BIT_ULL(KEY_TYPE_reservation) |
         BIT_ULL(KEY_TYPE_reflink_p) |
         BIT_ULL(KEY_TYPE_inline_data),
         "File data extent pointers and reservations"),
        (inodes,                1,
         BTREE_IS_snapshots,
         BIT_ULL(KEY_TYPE_whiteout) |
         BIT_ULL(KEY_TYPE_inode) |
         BIT_ULL(KEY_TYPE_inode_v2) |
         BIT_ULL(KEY_TYPE_inode_v3) |
         BIT_ULL(KEY_TYPE_inode_generation),
         "Inode metadata"),
        (dirents,               2,
         BTREE_IS_snapshots,
         BIT_ULL(KEY_TYPE_whiteout) |
         BIT_ULL(KEY_TYPE_hash_whiteout) |
         BIT_ULL(KEY_TYPE_dirent),
         "Directory entries"),
        (xattrs,                3,
         BTREE_IS_snapshots,
         BIT_ULL(KEY_TYPE_whiteout) |
         BIT_ULL(KEY_TYPE_cookie) |
         BIT_ULL(KEY_TYPE_hash_whiteout) |
         BIT_ULL(KEY_TYPE_xattr),
         "Extended attributes"),
        (alloc,                 4,  0,
         BIT_ULL(KEY_TYPE_alloc) |
         BIT_ULL(KEY_TYPE_alloc_v2) |
         BIT_ULL(KEY_TYPE_alloc_v3) |
         BIT_ULL(KEY_TYPE_alloc_v4),
         "Bucket allocation state"),
        (quotas,                5,  0,
         BIT_ULL(KEY_TYPE_quota),
         "User, group, and project quota counters"),
        (stripes,               6,
         BTREE_IS_data,
         BIT_ULL(KEY_TYPE_stripe),
         "Erasure coding stripe descriptors"),
        (reflink,               7,
         BTREE_IS_extents |
         BTREE_IS_data,
         BIT_ULL(KEY_TYPE_reflink_v) |
         BIT_ULL(KEY_TYPE_indirect_inline_data) |
         BIT_ULL(KEY_TYPE_error),
         "Reflink shared extent pointers"),
        (subvolumes,            8,  0,
         BIT_ULL(KEY_TYPE_subvolume),
         "Subvolume metadata"),
        (snapshots,             9,  0,
         BIT_ULL(KEY_TYPE_snapshot),
         "Snapshot tree structure"),
        (lru,                   10,
         BTREE_IS_write_buffer,
         BIT_ULL(KEY_TYPE_set),
         "Least-recently-used tracking for cache eviction"),
        (freespace,             11,
         BTREE_IS_extents,
         BIT_ULL(KEY_TYPE_set),
         "Free space index"),
        (need_discard,          12,
         BTREE_IS_write_buffer,
         BIT_ULL(KEY_TYPE_set),
         "Buckets waiting for discard/TRIM"),
        (backpointers,          13,
         BTREE_IS_write_buffer,
         BIT_ULL(KEY_TYPE_backpointer),
         "Reverse pointers from data extents back to btree nodes"),
        (bucket_gens,           14, 0,
         BIT_ULL(KEY_TYPE_bucket_gens),
         "Bucket generation numbers for stale pointer detection"),
        (snapshot_trees,        15, 0,
         BIT_ULL(KEY_TYPE_snapshot_tree),
         "Snapshot tree roots"),
        (deleted_inodes,        16,
         BTREE_IS_snapshot_field |
         BTREE_IS_write_buffer,
         BIT_ULL(KEY_TYPE_set),
         "Inodes pending deletion"),
        (logged_ops,            17, 0,
         BIT_ULL(KEY_TYPE_logged_op_truncate) |
         BIT_ULL(KEY_TYPE_logged_op_finsert) |
         BIT_ULL(KEY_TYPE_logged_op_stripe_update) |
         BIT_ULL(KEY_TYPE_logged_op_inode_opt_propagate) |
         BIT_ULL(KEY_TYPE_inode_alloc_cursor),
         "In-progress logged operations for crash recovery"),
        (reconcile_work,        18,
         BTREE_IS_snapshot_field |
         BTREE_IS_write_buffer,
         BIT_ULL(KEY_TYPE_set) | BIT_ULL(KEY_TYPE_cookie),
         "Reconcile work items"),
        (subvolume_children,    19, 0,
         BIT_ULL(KEY_TYPE_set),
         "Subvolume parent-child relationships"),
        (accounting,            20,
         BTREE_IS_snapshot_field |
         BTREE_IS_write_buffer,
         BIT_ULL(KEY_TYPE_accounting),
         "Space accounting by replicas and disk groups"),
        (reconcile_hipri,       21,
         BTREE_IS_snapshot_field |
         BTREE_IS_write_buffer,
         BIT_ULL(KEY_TYPE_set),
         "High-priority reconcile work items"),
        (reconcile_pending,     22,
         BTREE_IS_snapshot_field |
         BTREE_IS_write_buffer,
         BIT_ULL(KEY_TYPE_set),
         "Pending reconcile work items"),
        (reconcile_scan,        23, 0,
         BIT_ULL(KEY_TYPE_cookie) |
         BIT_ULL(KEY_TYPE_backpointer),
         "Reconcile scan state"),
        (reconcile_work_phys,   24,
         BTREE_IS_write_buffer,
         BIT_ULL(KEY_TYPE_set),
         "Physical reconcile work items"),
        (reconcile_hipri_phys,  25,
         BTREE_IS_write_buffer,
         BIT_ULL(KEY_TYPE_set),
         "Physical high-priority reconcile work items"),
        (bucket_to_stripe,      26, 0,
         BIT_ULL(KEY_TYPE_set),
         "Bucket to stripe mapping for erasure coding"),
        (stripe_backpointers,   27,
         BTREE_IS_write_buffer,
         BIT_ULL(KEY_TYPE_backpointer),
         "Stripe backpointers"),
        (damage,                28,
         BTREE_IS_snapshots,
         BIT_ULL(KEY_TYPE_whiteout) |
         BIT_ULL(KEY_TYPE_damage),
         "Inodes damaged by errors and repairs"),
    }
}

macro_rules! __btree_id_0 {
    ([$($acc:tt)*] $(($name:tt, $nr:expr$(, $($__rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum btree_id: u32 {
                $($acc)*
                $([<BTREE_ID_ $name>] = (($nr) as u32),)*
                BTREE_ID_NR,
            }
        }
    } };
}
BCH_BTREE_IDS!(__btree_id_0 []);

c_verbatim!(r#"
#include "alloc/replicas_format_gen.h"
#include "alloc/accounting_format_gen.h"
#include "alloc/accounting_format_inline.h"
#include "alloc/disk_groups_format_gen.h"
#include "alloc/lru_format_gen.h"
#include "alloc/format_gen.h"
#include "alloc/format_inline.h"
#include "data/reconcile/format_gen.h"
#include "data/extents_format_gen.h"
#include "data/ec/format_gen.h"
#include "data/extents_sb_format_gen.h"
#include "data/reflink_format_gen.h"
#include "fs/dirent_format_gen.h"
#include "fs/inode_format_gen.h"
#include "fs/logged_ops_format_gen.h"
#include "fs/quota_format_gen.h"
#include "fs/xattr_format_gen.h"
#include "sb/errors_format_gen.h"
#include "sb/errors_format_inline.h"
#include "init/damage_format_gen.h"
#include "init/passes_format_gen.h"
#include "init/passes_format_inline.h"
#include "journal/seq_blacklist_format_gen.h"
#include "sb/counters_format_gen.h"
#include "sb/counters_format_inline.h"
#include "sb/downgrade_format_gen.h"
#include "sb/members_format_gen.h"
#include "snapshots/format_gen.h"
"#);

macro_rules! __bch_sb_field_type_0 {
    ([$($acc:tt)*] $(($f:tt, $nr:expr$(, $($__rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_sb_field_type: u32 {
                $($acc)*
                $([<BCH_SB_FIELD_ $f>] = (($nr) as u32),)*
                BCH_SB_FIELD_NR,
            }
        }
    } };
}
BCH_SB_FIELDS!(__bch_sb_field_type_0 []);

c_const! {
    /*
     * Most superblock fields are replicated in all device's superblocks - a few are
     * not:
     */
    pub const BCH_SINGLE_DEVICE_SB_FIELDS: u32 =
        (1 << c::BCH_SB_FIELD_journal as u32) |
        (1 << c::BCH_SB_FIELD_journal_v2 as u32);
}

/* BCH_SB_FIELD_journal: */

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_sb_field_journal {
    pub field: c::bch_sb_field,
    pub buckets: [le::U64; 0],
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_sb_field_journal_v2_entry {
    pub start: le::U64,
    pub nr: le::U64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_sb_field_journal_v2 {
    pub field: c::bch_sb_field,

    pub d: [c::bch_sb_field_journal_v2_entry; 0],
}

/* BCH_SB_FIELD_crypt: */

#[repr(C, align(4))]
#[derive(Clone, Copy, Default, CStruct)]
pub struct nonce {
    pub d: [le::U32; 4],
}

#[repr(C, align(8))]
#[derive(Default, CStruct, TypeInfo)]
pub struct bch_key {
    pub key: [le::U64; 4],
}

c_verbatim!(r#"
#define BCH_KEY_MAGIC					\
	(((__u64) 'b' <<  0)|((__u64) 'c' <<  8)|		\
	 ((__u64) 'h' << 16)|((__u64) '*' << 24)|		\
	 ((__u64) '*' << 32)|((__u64) 'k' << 40)|		\
	 ((__u64) 'e' << 48)|((__u64) 'y' << 56))
"#);

#[repr(C)]
#[derive(Default, CStruct, TypeInfo)]
pub struct bch_encrypted_key {
    pub magic: le::U64,
    pub key: c::bch_key,
}

/*
 * If this field is present in the superblock, it stores an encryption key which
 * is used encrypt all other data/metadata. The key will normally be encrypted
 * with the key userspace provides, but if encryption has been turned off we'll
 * just store the master key unencrypted in the superblock so we can access the
 * previously encrypted data.
 */
#[repr(C)]
#[derive(Default, CStruct, TypeInfo)]
pub struct bch_sb_field_crypt {
    pub field: c::bch_sb_field,

    pub flags: le::U64,
    pub kdf_flags: le::U64,
    pub key: c::bch_encrypted_key,
}

c_bitmask! {
    LE64_BITMASK(struct bch_sb_field_crypt, flags) {
        BCH_CRYPT_KDF_TYPE(0, 4),
    }
}

c_enum! {
    #[open]
    pub enum bch_kdf_types: u32 {
        BCH_KDF_SCRYPT = 0,
        BCH_KDF_NR = 1,
    }
}

c_bitmask! {
    LE64_BITMASK(struct bch_sb_field_crypt, kdf_flags) {
        /* stored as base 2 log of scrypt params: */
        BCH_KDF_SCRYPT_N(0, 16),
        BCH_KDF_SCRYPT_R(16, 32),
        BCH_KDF_SCRYPT_P(32, 48),
    }
}

/*
 * On clean shutdown, store btree roots and current journal sequence number in
 * the superblock:
 */
#[repr(C)]
#[derive(Default, CStruct)]
pub struct jset_entry {
    pub u64s: le::U16,
    pub btree_id: u8,
    pub level: u8,
    pub type_: u8, /* designates what this jset holds */
    pub pad: [u8; 3],

    pub start: [c::bkey_i; 0],
    pub _data: [u64; 0],
}

#[repr(C)]
#[derive(Default, CStruct, TypeInfo)]
pub struct bch_sb_field_clean {
    pub field: c::bch_sb_field,

    pub flags: le::U32,
    pub _read_clock: le::U16, /* no longer used */
    pub _write_clock: le::U16,
    pub journal_seq: le::U64,

    pub start: [c::jset_entry; 0],
    pub _data: [u64; 0],
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_sb_field_ext {
    pub field: c::bch_sb_field,
    pub recovery_passes_required: [le::U64; 2],
    pub errors_silent: [le::U64; 8],
    pub btrees_lost_data: le::U64,
    pub flags0: le::U64,
    /*
     * Like btrees_lost_data, but never cleared - btrees_lost_data gates
     * reconstruction and is cleared when repair completes; this is the
     * forensic record of every btree that has ever lost data:
     */
    pub btrees_lost_data_ever: le::U64,
    /*
     * Btrees validated consistent by their check pass and not mutated
     * since. Written synchronously on every change (set on clean pass
     * completion, cleared from the btree's transactional trigger on
     * mutation) so the on-disk value is always current. Lets consistency
     * checks that would otherwise destroy data based on an in-memory table
     * they can't fully trust (check_key_has_snapshot) instead reschedule
     * the check pass. Runtime copy: bch_sb.btrees_clean.
     */
    pub btrees_clean: le::U64,
}

c_bitmask! {
    LE64_BITMASK(struct bch_sb_field_ext, flags0) {
        BCH_SB_EXT_DEV_READAHEAD(0, 20),
        BCH_SB_EXT_EC_STRIPE_BUF_LIMIT(20, 26),
        BCH_SB_EXT_SCRUB_MAX_REWIND_SECS(26, 38),
        BCH_SB_EXT_DISCARD_BUFFER(38, 42),
        BCH_SB_EXT_BTREE_CACHE_SHRINKER_SEEKS(42, 49),
        BCH_SB_EXT_MISSING_DEV_TIMEOUT(49, 61),
    }
}

c_verbatim!(r#"
/* Superblock: */

/*
 * New versioning scheme:
 * One common version number for all on disk data structures - superblock, btree
 * nodes, journal entries
 */
#define BCH_VERSION_MAJOR(_v)		((__u16) ((_v) >> 10))
"#);

#[allow(non_snake_case)]
pub const fn BCH_VERSION_MAJOR(v: u64) -> u64 { v >> 10 }

c_verbatim!(r#"
#define BCH_VERSION_MINOR(_v)		((__u16) ((_v) & ~(~0U << 10)))
"#);

#[allow(non_snake_case)]
pub const fn BCH_VERSION_MINOR(v: u64) -> u64 { v & !(!0 << 10) }

c_verbatim!(r#"
#define BCH_VERSION(_major, _minor)	(((_major) << 10)|(_minor) << 0)
"#);

#[allow(non_snake_case)]
pub const fn BCH_VERSION(major: u64, minor: u64) -> u64 { (major << 10) | (minor << 0) }

c_xmacro! {
    /*
     * field 1:		version name
     * field 2:		BCH_VERSION(major, minor)
     */
    BCH_METADATA_VERSIONS(x) {
        (bkey_renumber,                 BCH_VERSION(0, 10),
         "Renumbered bkey type values",                 "2018-11"),
        (inode_btree_change,            BCH_VERSION(0, 11),
         "Swapped inode/offset fields in bpos "
         "for better locality",                         "2020-03"),
        (snapshot,                      BCH_VERSION(0, 12),
         "Snapshot support: snapshot field in bpos, "
         "KEY_TYPE_snapshot, KEY_TYPE_subvolume",       "2021-03"),
        (inode_backpointers,            BCH_VERSION(0, 13),
         "Backpointers from extents to inodes",         "2021-04"),
        (btree_ptr_sectors_written,     BCH_VERSION(0, 14),
         "sectors_written field in btree_ptr_v2 for "
         "partial write tracking",                      "2021-07"),
        (snapshot_2,                    BCH_VERSION(0, 15),
         "Snapshot fixes: subvolume root inode tracking", "2021-09"),
        (reflink_p_fix,                 BCH_VERSION(0, 16),
         "Fixed reflink pointer refcount handling",     "2021-10"),
        (subvol_dirent,                 BCH_VERSION(0, 17),
         "Linked subvolumes to directory entries",      "2021-10"),
        (inode_v2,                      BCH_VERSION(0, 18),
         "KEY_TYPE_inode_v2 with improved field layout", "2021-11"),
        (freespace,                     BCH_VERSION(0, 19),
         "Freespace btree for faster allocation",       "2022-03"),
        (alloc_v4,                      BCH_VERSION(0, 20),
         "KEY_TYPE_alloc_v4 with comprehensive "
         "allocation metadata",                         "2022-03"),
        (new_data_types,                BCH_VERSION(0, 21),
         "Refined data type classifications "
         "in the allocator",                            "2022-04"),
        (backpointers,                  BCH_VERSION(0, 22),
         "Backpointers btree mapping physical locations "
         "back to logical extents",                     "2022-06"),
        (inode_v3,                      BCH_VERSION(0, 23),
         "KEY_TYPE_inode_v3 with compact encoding",     "2022-10"),
        (unwritten_extents,             BCH_VERSION(0, 24),
         "Preallocated extents that read as zeros",     "2022-11"),
        (bucket_gens,                   BCH_VERSION(0, 25),
         "bucket_gens btree for stale pointer detection", "2022-12"),
        (lru_v2,                        BCH_VERSION(0, 26),
         "Improved LRU tracking for cache eviction",    "2022-12"),
        (fragmentation_lru,             BCH_VERSION(0, 27),
         "LRU tracking for fragmented extents",         "2023-02"),
        (no_bps_in_alloc_keys,          BCH_VERSION(0, 28),
         "Backpointers moved fully to backpointers btree", "2023-03"),
        (snapshot_trees,                BCH_VERSION(0, 29),
         "snapshot_trees btree for hierarchical management", "2023-05"),
        (major_minor,                   BCH_VERSION(1,  0),
         "Major version bump: format stabilization, "
         "major.minor versioning scheme",               "2023-07"),
        (snapshot_skiplists,            BCH_VERSION(1,  1),
         "Skiplist structure for O(log n) ancestor queries", "2023-07"),
        (deleted_inodes,                BCH_VERSION(1,  2),
         "deleted_inodes btree for crash-safe unlink",  "2023-08"),
        (rebalance_work,                BCH_VERSION(1,  3),
         "Background rebalance work tracking",          "2023-10"),
        (member_seq,                    BCH_VERSION(1,  4),
         "Sequence numbers on member devices for "
         "stale superblock detection",                  "2023-12"),
        (subvolume_fs_parent,           BCH_VERSION(1,  5),
         "fs_parent field on subvolumes "
         "for path resolution",                         "2024-02"),
        (btree_subvolume_children,      BCH_VERSION(1,  6),
         "subvolume_children btree for "
         "parent-child lookups",                        "2024-02"),
        (mi_btree_bitmap,               BCH_VERSION(1,  7),
         "Per-device bitmap tracking which "
         "btrees have data",                            "2024-04"),
        (bucket_stripe_sectors,         BCH_VERSION(1,  8),
         "Stripe sector tracking in alloc keys "
         "for erasure coding",                          "2024-06"),
        (disk_accounting_v2,            BCH_VERSION(1,  9),
         "On-disk accounting btree for persistent "
         "space accounting",                            "2024-01"),
        (disk_accounting_v3,            BCH_VERSION(1, 10),
         "Accounting key validation and "
         "endianness fixes",                            "2024-08"),
        (disk_accounting_inum,          BCH_VERSION(1, 11),
         "Per-inode space accounting",                  "2024-08"),
        (rebalance_work_acct_fix,       BCH_VERSION(1, 12),
         "Fixed rebalance work accounting bugs",        "2024-08"),
        (inode_has_child_snapshots,     BCH_VERSION(1, 13),
         "Flag on inodes indicating child "
         "snapshot data exists",                        "2024-10"),
        (backpointer_bucket_gen,        BCH_VERSION(1, 14),
         "Bucket generation in backpointers for "
         "stale detection without alloc key reads",     "2024-11"),
        (disk_accounting_big_endian,    BCH_VERSION(1, 15),
         "Big-endian disk accounting fix",              "2024-11"),
        (reflink_p_may_update_opts,     BCH_VERSION(1, 16),
         "reflink_p keys carry independently "
         "updatable extent options",                    "2024-12"),
        (inode_depth,                   BCH_VERSION(1, 17),
         "Directory depth tracking in inodes",          "2024-12"),
        (persistent_inode_cursors,      BCH_VERSION(1, 18),
         "Persistent inode number allocation cursors",  "2024-12"),
        (autofix_errors,                BCH_VERSION(1, 19),
         "Default error action changed to fix_safe",    "2024-12"),
        (directory_size,                BCH_VERSION(1, 20),
         "Directory size tracking in inode metadata",   "2025-01"),
        (cached_backpointers,           BCH_VERSION(1, 21),
         "In-memory backpointer caching "
         "for faster lookups",                          "2025-02"),
        (stripe_backpointers,           BCH_VERSION(1, 22),
         "Backpointers for erasure-coded stripes",      "2025-02"),
        (stripe_lru,                    BCH_VERSION(1, 23),
         "LRU tracking for stripe cache eviction",      "2025-02"),
        (casefolding,                   BCH_VERSION(1, 24),
         "Case-insensitive filename support "
         "with per-directory flags",                    "2025-02"),
        (extent_flags,                  BCH_VERSION(1, 25),
         "Flags field on extent keys for "
         "extent-level metadata",                       "2025-03"),
        (snapshot_deletion_v2,          BCH_VERSION(1, 26),
         "Improved snapshot deletion with better "
         "interior node handling",                      "2025-05"),
        (fast_device_removal,           BCH_VERSION(1, 27),
         "Fast device removal when no stale pointers exist", "2025-05"),
        (inode_has_case_insensitive,    BCH_VERSION(1, 28),
         "Casefold flag on inodes for directory handling", "2025-05"),
        (extent_snapshot_whiteouts,     BCH_VERSION(1, 29),
         "KEY_TYPE_extent_whiteout for efficient "
         "snapshot extent deletion",                    "2025-08"),
        (31bit_dirent_offset,           BCH_VERSION(1, 30),
         "Extended directory entry offset to 31 bits",  "2025-08"),
        (btree_node_accounting,         BCH_VERSION(1, 31),
         "Per-btree-node space accounting",             "2025-09"),
        (sb_field_extent_type_u64s,     BCH_VERSION(1, 32),
         "Superblock field for extent type size limits", "2025-11"),
        (reconcile,                     BCH_VERSION(1, 33),
         "Reconcile system for IO options inheritance "
         "and background data movement",                "2025-11"),
        (extented_key_type_error,       BCH_VERSION(1, 34),
         "KEY_TYPE_error changed to zero-byte value",   "2025-12"),
        (bucket_stripe_index,           BCH_VERSION(1, 35),
         "Stripe index in alloc keys for efficient "
         "stripe-bucket lookups",                       "2026-01"),
        (no_sb_user_data_replicas,      BCH_VERSION(1, 36),
         "Removed redundant user_data replicas "
         "from superblock",                             "2026-01"),
        (erasure_coding,                BCH_VERSION(1, 37),
         "First release with fully supported erasure coding: all key "
         "functionality done, resilver integrated with reconcile", "2026-03"),
        (need_discard_by_journal_seq,   BCH_VERSION(1, 38),
         "need_discard btree reindexed by journal seq for O(1) "
         "discard eligibility checks",                  "2026-03"),
        (per_dev_fragmentation_lru,     BCH_VERSION(1, 39),
         "Per-device bucket fragmentation LRUs, so copygc can reason "
         "about fragmentation per device",              "2026-07"),
    }
}

macro_rules! __bcachefs_metadata_version_0 {
    ([$($acc:tt)*] $(($t:tt, $n:expr$(, $($__rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bcachefs_metadata_version: u32 {
                $($acc)*
                $([<bcachefs_metadata_version_ $t>] = (($n) as u32),)*
                bcachefs_metadata_version_max,
            }
        }
    } };
}
BCH_METADATA_VERSIONS!(__bcachefs_metadata_version_0 [bcachefs_metadata_version_min = 9,]);

c_verbatim!(r#"
static const __maybe_unused
unsigned bcachefs_metadata_required_upgrade_below = bcachefs_metadata_version_btree_node_accounting;
"#);

c_const! {
    #[c_int]
    pub const bcachefs_metadata_version_current: u32 = c::bcachefs_metadata_version_max as u32 - 1;
}

c_const! {
    #[c_int]
    pub const BCH_SB_SECTOR: u32 = 8;
}

c_const! {
    #[c_int]
    pub const BCH_SB_LAYOUT_SIZE_BITS_MAX: u32 = 16; /* 32 MB */
}

#[repr(C, align(8))]
#[derive(Clone, Copy, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_sb_layout {
    pub magic: c::__uuid_t, /* bcachefs superblock UUID */
    pub layout_type: u8,
    pub sb_max_size_bits: u8, /* base 2 of 512 byte sectors */
    pub nr_superblocks: u8,
    pub pad: [u8; 5],
    pub sb_offset: [le::U64; 61],
}
c_default!(bch_sb_layout);

c_const! {
    #[c_int]
    pub const BCH_SB_LAYOUT_SECTOR: u32 = 7;
}

/*
 * @offset	- sector where this sb was written
 * @version	- on disk format version
 * @version_min	- Oldest metadata version this filesystem contains; so we can
 *		  safely drop compatibility code and refuse to mount filesystems
 *		  we'd need it for
 * @magic	- identifies as a bcachefs superblock (BCHFS_MAGIC)
 * @seq		- incremented each time superblock is written
 * @uuid	- used for generating various magic numbers and identifying
 *                member devices, never changes
 * @user_uuid	- user visible UUID, may be changed
 * @label	- filesystem label
 * @seq		- identifies most recent superblock, incremented each time
 *		  superblock is written
 * @features	- enabled incompatible features
 */
#[repr(C, align(8))]
#[derive(Clone, Copy, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_sb {
    pub csum: c::bch_csum,
    pub version: le::U16,
    pub version_min: le::U16,
    pub pad: [le::U16; 2],
    pub magic: c::__uuid_t,
    pub uuid: c::__uuid_t,
    pub user_uuid: c::__uuid_t,
    pub label: [u8; c::BCH_SB_LABEL_SIZE as usize],
    pub offset: le::U64,
    pub seq: le::U64,

    pub block_size: le::U16,
    pub dev_idx: u8,
    pub nr_devices: u8,
    pub u64s: le::U32,

    pub time_base_lo: le::U64,
    pub time_base_hi: le::U32,
    pub time_precision: le::U32,

    pub flags: [le::U64; 7],
    pub write_time: le::U64,
    pub features: [le::U64; 2],
    pub compat: [le::U64; 2],

    pub layout: c::bch_sb_layout,

    pub start: [c::bch_sb_field; 0],
    pub _data: [le::U64; 0],
}
c_default!(bch_sb);

c_bitmask! {
    LE16_BITMASK(struct bch_sb, block_size), strip BCH_SB_ {
        /*
         * Flags:
         * BCH_SB_INITALIZED	- set on first mount
         * BCH_SB_CLEAN		- did we shut down cleanly? Just a hint, doesn't affect
         *			  behaviour of mount/recovery path:
         * BCH_SB_INODE_32BIT	- limit inode numbers to 32 bits
         * BCH_SB_128_BIT_MACS	- 128 bit macs instead of 80
         * BCH_SB_ENCRYPTION_TYPE - if nonzero encryption is enabled; overrides
         *			   DATA/META_CSUM_TYPE. Also indicates encryption
         *			   algorithm in use, if/when we get more than one
         */
        BCH_SB_BLOCK_SIZE(0, 16),
    }
}

c_bitmask! {
    LE64_BITMASK(struct bch_sb, flags[0]), strip BCH_SB_ {
        BCH_SB_INITIALIZED(0, 1),
        BCH_SB_CLEAN(1, 2),
        BCH_SB_CSUM_TYPE(2, 8),
        BCH_SB_ERROR_ACTION(8, 12),

        BCH_SB_BTREE_NODE_SIZE(12, 28),

        BCH_SB_GC_RESERVE(28, 33),
        BCH_SB_ROOT_RESERVE(33, 40),

        BCH_SB_META_CSUM_TYPE(40, 44),
        BCH_SB_DATA_CSUM_TYPE(44, 48),

        BCH_SB_META_REPLICAS_WANT(48, 52),
        BCH_SB_DATA_REPLICAS_WANT(52, 56),

        BCH_SB_POSIX_ACL(56, 57),
        BCH_SB_USRQUOTA(57, 58),
        BCH_SB_GRPQUOTA(58, 59),
        BCH_SB_PRJQUOTA(59, 60),

        BCH_SB_HAS_ERRORS(60, 61),
        BCH_SB_HAS_TOPOLOGY_ERRORS(61, 62),

        BCH_SB_BIG_ENDIAN(62, 63),
        BCH_SB_PROMOTE_WHOLE_EXTENTS(63, 64),
    }
}

c_bitmask! {
    LE64_BITMASK(struct bch_sb, flags[1]), strip BCH_SB_ {
        BCH_SB_STR_HASH_TYPE(0, 4),
        BCH_SB_COMPRESSION_TYPE_LO(4, 8),
        BCH_SB_INODE_32BIT(8, 9),

        BCH_SB_128_BIT_MACS(9, 10),
        BCH_SB_ENCRYPTION_TYPE(10, 14),

        /*
         * Max size of an extent that may require bouncing to read or write
         * (checksummed, compressed): 64k
         */
        BCH_SB_ENCODED_EXTENT_MAX_BITS(14, 20),

        BCH_SB_META_REPLICAS_REQ(20, 24),
        BCH_SB_DATA_REPLICAS_REQ(24, 28),

        BCH_SB_PROMOTE_TARGET(28, 40),
        BCH_SB_FOREGROUND_TARGET(40, 52),
        BCH_SB_BACKGROUND_TARGET(52, 64),
    }
}

c_bitmask! {
    LE64_BITMASK(struct bch_sb, flags[2]), strip BCH_SB_ {
        BCH_SB_BACKGROUND_COMPRESSION_TYPE_LO(0, 4),
        BCH_SB_GC_RESERVE_BYTES(4, 64),
    }
}

c_bitmask! {
    LE64_BITMASK(struct bch_sb, flags[3]), strip BCH_SB_ {
        BCH_SB_ERASURE_CODE(0, 16),
        BCH_SB_METADATA_TARGET(16, 28),
        BCH_SB_SHARD_INUMS(28, 29),
        BCH_SB_JOURNAL_FLUSH_DELAY(30, 62),
        BCH_SB_JOURNAL_FLUSH_DISABLED(62, 63),
        BCH_SB_MULTI_DEVICE(63, 64),
    }
}

c_bitmask! {
    LE64_BITMASK(struct bch_sb, flags[4]), strip BCH_SB_ {
        BCH_SB_JOURNAL_RECLAIM_DELAY(0, 32),
        BCH_SB_JOURNAL_TRANSACTION_NAMES(32, 33),
        BCH_SB_NOCOW(33, 34),
        BCH_SB_WRITE_BUFFER_SIZE(34, 54),
        BCH_SB_VERSION_UPGRADE(54, 56),

        BCH_SB_COMPRESSION_TYPE_HI(56, 60),
        BCH_SB_BACKGROUND_COMPRESSION_TYPE_HI(60, 64),
    }
}

c_bitmask! {
    LE64_BITMASK(struct bch_sb, flags[5]), strip BCH_SB_ {
        BCH_SB_VERSION_UPGRADE_COMPLETE(0, 16),
        BCH_SB_ALLOCATOR_STUCK_TIMEOUT(16, 32),
        BCH_SB_VERSION_INCOMPAT(32, 48),
        BCH_SB_VERSION_INCOMPAT_ALLOWED(48, 64),
    }
}

c_bitmask! {
    LE64_BITMASK(struct bch_sb, flags[6]), strip BCH_SB_ {
        BCH_SB_SHARD_INUMS_NBITS(0, 4),
        BCH_SB_WRITE_ERROR_TIMEOUT(4, 14),
        BCH_SB_CSUM_ERR_RETRY_NR(14, 20),
        BCH_SB_DEGRADED_ACTION(20, 22),
        BCH_SB_CASEFOLD(22, 23),
        BCH_SB_REBALANCE_AC_ONLY(23, 24),
        BCH_SB_WRITEBACK_TIMEOUT(24, 40),
        BCH_SB_EXTENT_BP_SHIFT(40, 48),
        BCH_SB_SCRUB_JOURNAL(48, 50),
        BCH_SB_EC_MAX_DATA_BLOCKS(50, 58),
        BCH_SB_MOVE_WRITES_FUA(58, 59),
        /*
         * Set by `bcachefs dump --sanitize` when it scrubs dirent names: the names
         * (and therefore their str_hash positions) are meaningless, so fsck must skip
         * the dirent hash-consistency check rather than "repair" the artifacts.
         */
        BCH_SB_DIRENTS_SANITIZED(59, 60),
        BCH_SB_WRITE_DEGRADED_ACTION(60, 62),
    }
}

c_const! {
    #[c_int]
    pub const BCH_SB_EXTENT_BP_SHIFT_DEFAULT: u32 = 10;
}

c_xmacro! {
    /*
     * Features:
     *
     * journal_seq_blacklist_v3:	gates BCH_SB_FIELD_journal_seq_blacklist
     * reflink:			gates KEY_TYPE_reflink
     * inline_data:			gates KEY_TYPE_inline_data
     * new_siphash:			gates BCH_STR_HASH_siphash
     * new_extent_overwrite:	gates BTREE_NODE_NEW_EXTENT_OVERWRITE
     */
    BCH_SB_FEATURES(x) {
        (lz4, 0),
        (gzip, 1),
        (zstd, 2),
        (atomic_nlink, 3),
        (ec, 4),
        (journal_seq_blacklist_v3, 5),
        (reflink, 6),
        (new_siphash, 7),
        (inline_data, 8),
        (new_extent_overwrite, 9),
        (incompressible, 10),
        (btree_ptr_v2, 11),
        (extents_above_btree_updates, 12),
        (btree_updates_journalled, 13),
        (reflink_inline_data, 14),
        (new_varint, 15),
        (journal_no_flush, 16),
        (alloc_v2, 17),
        (extents_across_btree_nodes, 18),
        (incompat_version_field, 19),
        (casefolding, 20),
        (no_alloc_info, 21),
        (small_image, 22),
        (no_default_sb, 23),
    }
}

c_const! {
    pub const BCH_SB_FEATURES_ALWAYS: u64 =
        (1 << c::BCH_FEATURE_new_extent_overwrite as u32) |
        (1 << c::BCH_FEATURE_extents_above_btree_updates as u32) |
        (1 << c::BCH_FEATURE_btree_updates_journalled as u32) |
        (1 << c::BCH_FEATURE_alloc_v2 as u32) |
        (1 << c::BCH_FEATURE_extents_across_btree_nodes as u32);
}

c_const! {
    pub const BCH_SB_FEATURES_ALL: u64 =
        c::BCH_SB_FEATURES_ALWAYS |
        (1 << c::BCH_FEATURE_new_siphash as u32) |
        (1 << c::BCH_FEATURE_btree_ptr_v2 as u32) |
        (1 << c::BCH_FEATURE_new_varint as u32) |
        (1 << c::BCH_FEATURE_journal_no_flush as u32) |
        (1 << c::BCH_FEATURE_incompat_version_field as u32);
}

macro_rules! __bch_sb_feature_0 {
    ([$($acc:tt)*] $(($f:tt$(, $($__rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_sb_feature: u32 {
                $($acc)*
                $([<BCH_FEATURE_ $f>],)*
                BCH_FEATURE_NR,
            }
        }
    } };
}
BCH_SB_FEATURES!(__bch_sb_feature_0 []);

c_xmacro! {
    BCH_SB_COMPAT(x) {
        (alloc_info, 0),
        (alloc_metadata, 1),
        (extents_above_btree_updates_done, 2),
        (bformat_overflow_done, 3),
        (no_stale_ptrs, 4),
        (stripe_frag_accounting, 5),
        (inode_opts_propagated, 6),
    }
}

macro_rules! __bch_sb_compat_0 {
    ([$($acc:tt)*] $(($f:tt$(, $($__rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_sb_compat: u32 {
                $($acc)*
                $([<BCH_COMPAT_ $f>],)*
                BCH_COMPAT_NR,
            }
        }
    } };
}
BCH_SB_COMPAT!(__bch_sb_compat_0 []);

c_xmacro! {
    /* options: */
    BCH_VERSION_UPGRADE_OPTS(x) {
        (compatible, 0),
        (incompatible, 1),
        (none, 2),
    }
}

macro_rules! __bch_version_upgrade_opts_0 {
    ([$($acc:tt)*] $(($t:tt, $n:expr)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_version_upgrade_opts: u32 {
                $($acc)*
                $([<BCH_VERSION_UPGRADE_ $t>] = (($n) as u32),)*
            }
        }
    } };
}
BCH_VERSION_UPGRADE_OPTS!(__bch_version_upgrade_opts_0 []);

c_const! {
    pub const BCH_REPLICAS_MAX: u32 = 4;
}

c_const! {
    pub const BCH_BKEY_PTRS_MAX: u32 = 16;
}

c_xmacro! {
    BCH_ERROR_ACTIONS(x) {
        (continue,              0,
         "Log the error but continue normal operation "
         "without attempting repair"),
        (fix_safe,              1,
         "Automatically repair errors that are safe to fix "
         "without user confirmation. Unsafe errors cause "
         "emergency read-only with a message to run fsck."),
        (panic,                 2,
         "Immediately halt the entire machine, printing a "
         "backtrace on the system console"),
        (ro,                    3,
         "Emergency read-only, immediately halting any "
         "changes to the filesystem on disk"),
    }
}

macro_rules! __bch_error_actions_0 {
    ([$($acc:tt)*] $(($t:tt, $n:expr$(, $($__rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_error_actions: u32 {
                $($acc)*
                $([<BCH_ON_ERROR_ $t>] = (($n) as u32),)*
                BCH_ON_ERROR_NR,
            }
        }
    } };
}
BCH_ERROR_ACTIONS!(__bch_error_actions_0 []);

c_xmacro! {
    BCH_DEGRADED_ACTIONS(x) {
        (ask, 0),
        (yes, 1),
        (very, 2),
        (no, 3),
    }
}

macro_rules! __bch_degraded_actions_0 {
    ([$($acc:tt)*] $(($t:tt, $n:expr)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_degraded_actions: u32 {
                $($acc)*
                $([<BCH_DEGRADED_ $t>] = (($n) as u32),)*
                BCH_DEGRADED_ACTIONS_NR,
            }
        }
    } };
}
BCH_DEGRADED_ACTIONS!(__bch_degraded_actions_0 []);

c_xmacro! {
    /*
     * What to do when a write can't be placed at the replica count it asked for:
     * refuse it (-ENOSPC), or reserve at the count we can place at.
     *
     * @degraded is the default because the two reasons we can't place differ in
     * kind. A device that is gone will come back, or be replaced, and refusing
     * writes until then is worse than writing fewer copies; a filesystem whose
     * devices are all present and simply can't hold another copy is the shape the
     * user built, and the honest answer there is -ENOSPC rather than quietly
     * dropping below the replica count they asked for.
     */
    BCH_WRITE_DEGRADED_ACTIONS(x) {
        (degraded, 0),
        (yes, 1),
        (no, 2),
    }
}

macro_rules! __bch_write_degraded_actions_0 {
    ([$($acc:tt)*] $(($t:tt, $n:expr)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_write_degraded_actions: u32 {
                $($acc)*
                $([<BCH_WRITE_DEGRADED_ $t>] = (($n) as u32),)*
                BCH_WRITE_DEGRADED_ACTIONS_NR,
            }
        }
    } };
}
BCH_WRITE_DEGRADED_ACTIONS!(__bch_write_degraded_actions_0 []);

c_xmacro! {
    BCH_STR_HASH_TYPES(x) {
        (crc32c, 0),
        (crc64, 1),
        (siphash_old, 2),
        (siphash, 3),
    }
}

macro_rules! __bch_str_hash_type_0 {
    ([$($acc:tt)*] $(($t:tt, $n:expr)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_str_hash_type: u32 {
                $($acc)*
                $([<BCH_STR_HASH_ $t>] = (($n) as u32),)*
                BCH_STR_HASH_NR,
            }
        }
    } };
}
BCH_STR_HASH_TYPES!(__bch_str_hash_type_0 []);

c_xmacro! {
    BCH_STR_HASH_OPTS(x) {
        (crc32c, 0),
        (crc64, 1),
        (siphash, 2),
    }
}

macro_rules! __bch_str_hash_opts_0 {
    ([$($acc:tt)*] $(($t:tt, $n:expr)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_str_hash_opts: u32 {
                $($acc)*
                $([<BCH_STR_HASH_OPT_ $t>] = (($n) as u32),)*
                BCH_STR_HASH_OPT_NR,
            }
        }
    } };
}
BCH_STR_HASH_OPTS!(__bch_str_hash_opts_0 []);

c_xmacro! {
    BCH_CSUM_TYPES(x) {
        (none, 0),
        (crc32c_nonzero, 1),
        (crc64_nonzero, 2),
        (chacha20_poly1305_80, 3),
        (chacha20_poly1305_128, 4),
        (crc32c, 5),
        (crc64, 6),
        (xxhash, 7),
    }
}

macro_rules! __bch_csum_type_0 {
    ([$($acc:tt)*] $(($t:tt, $n:expr)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_csum_type: u32 {
                $($acc)*
                $([<BCH_CSUM_ $t>] = (($n) as u32),)*
                BCH_CSUM_NR,
            }
        }
    } };
}
BCH_CSUM_TYPES!(__bch_csum_type_0 []);

c_verbatim!(r#"
static const __maybe_unused unsigned bch_crc_bytes[] = {
	[BCH_CSUM_none]				= 0,
	[BCH_CSUM_crc32c_nonzero]		= 4,
	[BCH_CSUM_crc32c]			= 4,
	[BCH_CSUM_crc64_nonzero]		= 8,
	[BCH_CSUM_crc64]			= 8,
	[BCH_CSUM_xxhash]			= 8,
	[BCH_CSUM_chacha20_poly1305_80]		= 10,
	[BCH_CSUM_chacha20_poly1305_128]	= 16,
};
"#);

c_xmacro! {
    BCH_CSUM_OPTS(x) {
        (none, 0),
        (crc32c, 1),
        (crc64, 2),
        (xxhash, 3),
    }
}

macro_rules! __bch_csum_opt_0 {
    ([$($acc:tt)*] $(($t:tt, $n:expr)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_csum_opt: u32 {
                $($acc)*
                $([<BCH_CSUM_OPT_ $t>] = (($n) as u32),)*
                BCH_CSUM_OPT_NR,
            }
        }
    } };
}
BCH_CSUM_OPTS!(__bch_csum_opt_0 []);

c_xmacro! {
    BCH_COMPRESSION_TYPES(x) {
        (none, 0),
        (lz4_old, 1),
        (gzip, 2),
        (lz4, 3),
        (zstd, 4),
        (incompressible, 5),
    }
}

macro_rules! __bch_compression_type_0 {
    ([$($acc:tt)*] $(($t:tt, $n:expr)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_compression_type: u32 {
                $($acc)*
                $([<BCH_COMPRESSION_TYPE_ $t>] = (($n) as u32),)*
                BCH_COMPRESSION_TYPE_NR,
            }
        }
    } };
}
BCH_COMPRESSION_TYPES!(__bch_compression_type_0 []);

c_xmacro! {
    BCH_COMPRESSION_OPTS(x) {
        (none, 0),
        (lz4, 1),
        (gzip, 2),
        (zstd, 3),
    }
}

macro_rules! __bch_compression_opts_0 {
    ([$($acc:tt)*] $(($t:tt, $n:expr)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_compression_opts: u32 {
                $($acc)*
                $([<BCH_COMPRESSION_OPT_ $t>] = (($n) as u32),)*
                BCH_COMPRESSION_OPT_NR,
            }
        }
    } };
}
BCH_COMPRESSION_OPTS!(__bch_compression_opts_0 []);

c_xmacro! {
    BCH_SCRUB_JOURNAL_OPTS(x) {
        (unclean, 0),
        (no, 1),
        (always, 2),
    }
}

macro_rules! __bch_scrub_journal_opts_0 {
    ([$($acc:tt)*] $(($t:tt, $n:expr)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_scrub_journal_opts: u32 {
                $($acc)*
                $([<BCH_SCRUB_JOURNAL_ $t>] = (($n) as u32),)*
                BCH_SCRUB_JOURNAL_OPT_NR,
            }
        }
    } };
}
BCH_SCRUB_JOURNAL_OPTS!(__bch_scrub_journal_opts_0 []);

c_verbatim!(r#"
/*
 * Magic numbers
 *
 * The various other data structures have their own magic numbers, which are
 * xored with the first part of the cache set's UUID
 */

#define BCACHE_MAGIC							\
	UUID_INIT(0xc68573f6, 0x4e1a, 0x45ca,				\
		  0x82, 0x65, 0xf5, 0x7f, 0x48, 0xba, 0x6d, 0x81)
#define BCHFS_MAGIC							\
	UUID_INIT(0xc68573f6, 0x66ce, 0x90a9,				\
		  0xd9, 0x6a, 0x60, 0xcf, 0x80, 0x3d, 0xf7, 0xef)
"#);

c_const! {
    /* statfs()'s f_type, and the superblock's s_magic: the kernel's
     * uapi/linux/magic.h has it too, as BCACHEFS_SUPER_MAGIC */
    pub const BCACHEFS_STATFS_MAGIC: u32 = 0xca451a4e;
}

c_verbatim!(r#"
#define JSET_MAGIC		__cpu_to_le64(0x245235c1a3625032ULL)
#define BSET_MAGIC		__cpu_to_le64(0x90135c78b99e07f5ULL)
"#);

c_const! {
    /* Journal */
    pub const JSET_KEYS_U64s: usize = size_of::<c::jset_entry>() / size_of::<u64>();
}

c_xmacro! {
    BCH_JSET_ENTRY_TYPES(x) {
        (btree_keys,            0,
         "Btree key updates"),
        (btree_root,            1,
         "Btree root pointers, recorded every journal write"),
        (prio_ptrs,             2,
         "Legacy, no longer used"),
        (blacklist,             3,
         "Blacklist a single journal sequence number"),
        (blacklist_v2,          4,
         "Blacklist a range of journal sequence numbers"),
        (usage,                 5,
         "Maximum key version for encryption nonce "
         "derivation"),
        (data_usage,            6,
         "Legacy: usage accounting moved to accounting btree"),
        (clock,                 7,
         "IO clock: total reads and writes in sectors "
         "since filesystem creation"),
        (dev_usage,             8,
         "Legacy: per-device usage moved to accounting btree"),
        (log,                   9,
         "Free-form log message for fsck actions "
         "and diagnostic events"),
        (overwrite,             10,
         "Old value being overwritten, for debugging "
         "and journal_rewind"),
        (write_buffer_keys,     11,
         "Write buffer keys, transformed to btree_keys "
         "before writing to disk"),
        (datetime,              12,
         "Wall clock time at journal write"),
        (log_bkey,              13,
         "Structured log entry containing a btree key"),
        (rewind_limit,          14,
         "Oldest journal seq safe for rewind "
         "(discards may have invalidated earlier seqs)"),
        (rewind,                15,
         "Rewind in progress: keys from entries in this "
         "seq range use overwrite entries"),
    }
}

macro_rules! __bch_jset_entry_type_0 {
    ([$($acc:tt)*] $(($f:tt, $nr:expr$(, $($__rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_jset_entry_type: u32 {
                $($acc)*
                $([<BCH_JSET_ENTRY_ $f>] = (($nr) as u32),)*
                BCH_JSET_ENTRY_NR,
            }
        }
    } };
}
BCH_JSET_ENTRY_TYPES!(__bch_jset_entry_type_0 []);

/*
 * Journal sequence numbers can be blacklisted: bsets record the max sequence
 * number of all the journal entries they contain updates for, so that on
 * recovery we can ignore those bsets that contain index updates newer that what
 * made it into the journal.
 *
 * This means that we can't reuse that journal_seq - we have to skip it, and
 * then record that we skipped it so that the next time we crash and recover we
 * don't think there was a missing journal entry.
 */
#[repr(C)]
#[derive(Default, CStruct)]
pub struct jset_entry_blacklist {
    pub entry: c::jset_entry,
    #[c_anon("")] pub __seq_align: [u64; 0],
    pub seq: le::U64,
}

#[repr(C)]
#[derive(Default, CStruct)]
pub struct jset_entry_blacklist_v2 {
    pub entry: c::jset_entry,
    #[c_anon("")] pub __start_align: [u64; 0],
    pub start: le::U64,
    #[c_anon("")] pub __end_align: [u64; 0],
    pub end: le::U64,
}

c_xmacro! {
    BCH_FS_USAGE_TYPES(x) {
        (reserved, 0),
        (inodes, 1),
        (key_version, 2),
    }
}

macro_rules! __bch_fs_usage_type_0 {
    ([$($acc:tt)*] $(($f:tt, $nr:expr)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_fs_usage_type: u32 {
                $($acc)*
                $([<BCH_FS_USAGE_ $f>] = (($nr) as u32),)*
                BCH_FS_USAGE_NR,
            }
        }
    } };
}
BCH_FS_USAGE_TYPES!(__bch_fs_usage_type_0 []);

#[repr(C, packed)]
#[derive(Default, CStruct)]
pub struct jset_entry_usage {
    pub entry: c::jset_entry,
    pub v: le::U64,
}

#[repr(C, packed)]
#[derive(Default, CStruct)]
pub struct jset_entry_data_usage {
    pub entry: c::jset_entry,
    pub v: le::U64,
    pub r: c::bch_replicas_entry_v1,
}

#[repr(C, packed)]
#[derive(Default, CStruct)]
pub struct jset_entry_clock {
    pub entry: c::jset_entry,
    pub rw: u8,
    pub pad: [u8; 7],
    pub time: le::U64,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct jset_entry_dev_usage_type {
    pub buckets: le::U64,
    pub sectors: le::U64,
    pub fragmented: le::U64,
}

#[repr(C)]
#[derive(Default, CStruct)]
pub struct jset_entry_dev_usage {
    pub entry: c::jset_entry,
    #[c_anon("")] pub __dev_align: [u32; 0],
    pub dev: le::U32,
    pub pad: u32,

    #[c_anon("")] pub ___buckets_ec_align: [u64; 0],
    pub _buckets_ec: le::U64, /* No longer used */
    #[c_anon("")] pub ___buckets_unavailable_align: [u64; 0],
    pub _buckets_unavailable: le::U64, /* No longer used */

    pub d: [c::jset_entry_dev_usage_type; 0],
}

#[repr(C, align(8))]
#[derive(Default, CStruct)]
#[c_packed]
pub struct jset_entry_log {
    pub entry: c::jset_entry,
    pub d: [u8; 0],
}

#[repr(C, align(8))]
#[derive(Default, CStruct)]
#[c_packed]
pub struct jset_entry_datetime {
    pub entry: c::jset_entry,
    pub seconds: le::U64,
}

#[repr(C, align(8))]
#[derive(Default, CStruct)]
#[c_packed]
pub struct jset_entry_rewind_limit {
    pub entry: c::jset_entry,
    pub seq: le::U64,
}

/*
 * Records a journal rewind in progress. Keys from journal entries
 * with seq in (to, from] use overwrite entries instead of btree_keys.
 */
#[repr(C, align(8))]
#[derive(Default, CStruct)]
#[c_packed]
pub struct jset_entry_rewind {
    pub entry: c::jset_entry,
    pub from: le::U64,
    pub to: le::U64,
}

/*
 * On disk format for a journal entry:
 * seq is monotonically increasing; every journal entry has its own unique
 * sequence number.
 *
 * last_seq is the oldest journal entry that still has keys the btree hasn't
 * flushed to disk yet.
 *
 * version is for on disk format changes.
 */
#[repr(C, align(8))]
#[derive(Default, CStruct)]
#[c_packed]
pub struct jset {
    pub csum: c::bch_csum,

    pub magic: le::U64,
    pub seq: le::U64,
    pub version: le::U32,
    pub flags: le::U32,

    pub u64s: le::U32, /* size of d[] in u64s */

    pub encrypted_start: [u8; 0],

    pub _read_clock: le::U16, /* no longer used */
    pub _write_clock: le::U16,

    /* Sequence number of oldest dirty journal entry */
    pub last_seq: le::U64,

    pub start: [c::jset_entry; 0],
    pub _data: [u64; 0],
}

c_bitmask! {
    LE32_BITMASK(struct jset, flags), strip JSET_ {
        JSET_CSUM_TYPE(0, 4),
        JSET_BIG_ENDIAN(4, 5),
        JSET_NO_FLUSH(5, 6),
        JSET_HAS_OVERWRITES(6, 7),
    }
}

c_const! {
    #[c_int]
    pub const BCH_JOURNAL_BUCKETS_MIN: u32 = 8;
}

c_const! {
    /* Btree: */

    /*
     * Maximum number of btrees that we will _ever_ have under the current scheme,
     * where we refer to them with 64 bit bitfields - and we also need a bit for
     * the interior btree node type:
     */
    #[c_int]
    pub const BTREE_ID_NR_MAX: u32 = 63;
}

c_enum! {
    #[closed]
    pub enum _: u32 {
        BTREE_MAX_DEPTH = 4,
    }
}

/* Btree nodes */

/*
 * Btree nodes
 *
 * On disk a btree node is a list/log of these; within each set the keys are
 * sorted
 */
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct)]
#[c_packed]
pub struct bset {
    pub seq: le::U64,

    /*
     * Highest journal entry this bset contains keys for.
     * If on recovery we don't see that journal entry, this bset is ignored:
     * this allows us to preserve the order of all index updates after a
     * crash, since the journal records a total order of all index updates
     * and anything that didn't make it to the journal doesn't get used.
     */
    pub journal_seq: le::U64,

    pub flags: le::U32,
    pub version: le::U16,
    pub u64s: le::U16, /* count of d[] in u64s */

    pub start: [c::bkey_packed; 0],
    pub _data: [u64; 0],
}

c_bitmask! {
    LE32_BITMASK(struct bset, flags), strip BSET_ {
        BSET_CSUM_TYPE(0, 4),

        BSET_BIG_ENDIAN(4, 5),
        BSET_SEPARATE_WHITEOUTS(5, 6),

        /* Sector offset within the btree node: */
        BSET_OFFSET(16, 32),
    }
}

nest! {
    #[derive(Clone, Copy, CStruct)]*
    #[repr(C, align(8))]
    #[c_packed]
    pub struct btree_node {
        pub csum: c::bch_csum,
        pub magic: le::U64,

        /* this flags field is encrypted, unlike bset->flags: */
        pub flags: le::U64,

        /* Closed interval: */
        pub min_key: c::bpos,
        pub max_key: c::bpos,
        pub _ptr: c::bch_extent_ptr, /* not used anymore */
        pub format: c::bkey_format,

        #[c_anon]
        #>[repr(C)]
        pub bset: pub union btree_node_bset {
            pub keys: c::bset,
            #[c_anon]
            #>[repr(C)]
            pub contents: pub struct btree_node_bset_contents {
                pub pad: [u8; 22],
                pub u64s: le::U16,
                pub _data: [u64; 0],
            },
        },
    }
}
c_default!(btree_node);

c_bitmask! {
    LE64_BITMASK(struct btree_node, flags), strip BTREE_NODE_ {
        BTREE_NODE_ID_LO(0, 4),
        BTREE_NODE_LEVEL(4, 8),
        BTREE_NODE_NEW_EXTENT_OVERWRITE(8, 9),
        BTREE_NODE_ID_HI(9, 25),
        /* 25-32 unused */
        BTREE_NODE_SEQ(32, 64),
    }
}

nest! {
    #[derive(Clone, Copy, CStruct)]*
    #[repr(C, align(8))]
    #[c_packed]
    pub struct btree_node_entry {
        pub csum: c::bch_csum,

        #[c_anon]
        #>[repr(C)]
        pub bset: pub union btree_node_entry_bset {
            pub keys: c::bset,
            #[c_anon]
            #>[repr(C)]
            pub contents: pub struct btree_node_entry_bset_contents {
                pub pad: [u8; 22],
                pub u64s: le::U16,
                pub _data: [u64; 0],
            },
        },
    }
}
c_default!(btree_node_entry);
