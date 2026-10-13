// SPDX-License-Identifier: GPL-2.0

//! bcachefs on disk data structures
//!
//! OVERVIEW:
//!
//! There are three main types of on disk data structures in bcachefs (this is
//! reduced from 5 in bcache)
//!
//!  - superblock
//!  - journal
//!  - btree
//!
//! The btree is the primary structure; most metadata exists as keys in the
//! various btrees. There are only a small number of btrees, they're not
//! sharded - we have one btree for extents, another for inodes, et cetera.
//!
//! SUPERBLOCK:
//!
//! The superblock contains the location of the journal, the list of devices in
//! the filesystem, and in general any metadata we need in order to decide
//! whether we can start a filesystem or prior to reading the journal/btree
//! roots.
//!
//! The superblock is extensible, and most of the contents of the superblock are
//! in variable length, type tagged fields; see struct bch_sb_field.
//!
//! Backup superblocks do not reside in a fixed location; also, superblocks do
//! not have a fixed size. To locate backup superblocks we have struct
//! bch_sb_layout; we store a copy of this inside every superblock, and also
//! before the first superblock.
//!
//! JOURNAL:
//!
//! The journal primarily records btree updates in the order they occurred;
//! journal replay consists of just iterating over all the keys in the open
//! journal entries and re-inserting them into the btrees.
//!
//! The journal also contains entry types for the btree roots, and blacklisted
//! journal sequence numbers (see journal_seq_blacklist.c).
//!
//! BTREE:
//!
//! bcachefs btrees are copy on write b+ trees, where nodes are big (typically
//! 128k-256k) and log structured. We use struct btree_node for writing the first
//! entry in a given node (offset 0), and struct btree_node_entry for all
//! subsequent writes.
//!
//! After the header, btree node entries contain a list of keys in sorted order.
//! Values are stored inline with the keys; since values are variable length (and
//! keys effectively are variable length too, due to packing) we can't do random
//! access without building up additional in memory tables in the btree node read
//! path.
//!
//! BTREE KEYS (struct bkey):
//!
//! The various btrees share a common format for the key - so as to avoid
//! switching in fastpath lookup/comparison code - but define their own
//! structures for the key values.
//!
//! The size of a key/value pair is stored as a u8 in units of u64s, so the max
//! size is just under 2k. The common part also contains a type tag for the
//! value, and a format field indicating whether the key is packed or not (and
//! also meant to allow adding new key fields in the future, if desired).
//!
//! bkeys, when stored within a btree node, may also be packed. In that case, the
//! bkey_format in that node is used to unpack it. Packed bkeys mean that we can
//! be generous with field sizes in the common part of the key format (64 bit
//! inode number, 64 bit offset, 96 bit version field, etc.) for negligible cost.

#![allow(non_camel_case_types)]


use crate::cstructs::c;
use crate::types::{c_default, Le16, Le32, Le64};
use crate::util::vstructs::{floored_by_self, vstruct, vstruct_container, whole_arm, Ffi, FlexArray};
use cstruct_macros::{bitfield, c_bitmask, c_const, c_enum, c_verbatim, c_xmacro, grown, tagged_union,
                     unsized_vstruct, CStruct, Plain, TypeInfo, View};
#[cfg(__KERNEL__)]
use cstruct_macros::c_typedef;

#[cfg(__KERNEL__)]
c_typedef! {
    pub type __uuid_t = c::uuid_t;
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bkey_format {
    pub key_u64s: u8,
    pub nr_fields: u8,
    /* One unused slot for now: */
    pub bits_per_field: [u8; 6],
    pub field_offset: [Le64; 6],
}

/* Btree keys - all units are in sectors */

/*
 * Word order matches machine byte order - btree code treats a bpos as a
 * single large integer, for search/comparison purposes
 *
 * Note that wherever a bpos is embedded in another on disk data
 * structure, it has to be byte swabbed when reading in metadata that
 * wasn't written in native endian order:
 */
#[cfg(target_endian = "little")]
#[repr(C, packed(4))]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bpos {
    pub snapshot: u32,
    pub offset: u64,
    pub inode: u64,
}

#[cfg(target_endian = "big")]
#[repr(C, packed)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bpos {
    pub inode: u64,
    pub offset: u64, /* Points to end of extent - sectors */
    pub snapshot: u32,
}

/* Empty placeholder struct, for container_of() */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bch_val {
    pub __nothing: [u64; 0],
}

#[cfg(target_endian = "little")]
#[repr(C, packed(4))]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bversion {
    pub lo: u64,
    pub hi: u32,
}

#[cfg(target_endian = "big")]
#[repr(C, packed)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bversion {
    pub hi: u32,
    pub lo: u64,
}

#[bitfield(u8)]
pub struct bkey_format_bits {
    format:         u7,
    needs_whiteout: u1,
}

/*
 * The big-endian bkey has no align(8): rustc doesn't allow a type to be both
 * packed and aligned. The little-endian one can have it, because packed is
 * redundant there.
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
#[cfg(target_endian = "little")]
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
#[c_packed]
pub struct bkey {
    /* Size of combined key and value, in u64s */
    pub u64s: u8,

    /* Format of key (0 for format local to btree node) */
    #[c_bitfield]
    pub format_bits: bkey_format_bits,

    /* Type of the value */
    pub type_: u8,

    pub pad: [u8; 1],

    pub bversion: c::bversion,
    pub size: u32, /* extent size, in sectors */
    pub p: c::bpos,
}

#[cfg(target_endian = "big")]
#[repr(C, packed)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
#[c_packed]
pub struct bkey {
    /* Size of combined key and value, in u64s */
    pub u64s: u8,

    /* Format of key (0 for format local to btree node) */
    #[c_bitfield]
    pub format_bits: bkey_format_bits,

    /* Type of the value */
    pub type_: u8,

    pub p: c::bpos,
    pub size: u32, /* extent size, in sectors */
    pub bversion: c::bversion,

    pub pad: [u8; 1],
}

#[repr(C, align(8))]
#[derive(Clone, Copy, CStruct, Plain, TypeInfo)]
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
    pub format_bits: bkey_format_bits,

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
vstruct!(bkey_packed, u64s, start = 0);
c_default!(bkey_packed);

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
#[c_typedef]
pub struct bch_le128 {
    pub lo: Le64,
    pub hi: Le64,
}

c_const! {
    pub const BKEY_U64s: usize = size_of::<c::bkey>() / size_of::<u64>();
    pub const BKEY_U64s_MAX: u8 = u8::MAX;
    pub const BKEY_VAL_U64s_MAX: usize = c::BKEY_U64s_MAX as usize - c::BKEY_U64s;

    #[c_int]
    pub const KEY_PACKED_BITS_START: u32 = 24;

    #[c_int]
    pub const KEY_FORMAT_LOCAL_BTREE: u32 = 0;
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

/* bkey with inline value */
#[repr(C)]
#[derive(Default, CStruct, Plain, TypeInfo)]
pub struct bkey_i {
    pub _data: [u64; 0],

    pub k: c::bkey,
    pub v: c::bch_val,
}

/// _data, as long as k.u64s says - its own count is its bkey's, a field of a
/// field, which #[vstruct] doesn't take: SelfSized::bytes() is bkey_bytes().
impl FlexArray for bkey_i {
    type Elem = u64;
    const TAIL: usize = core::mem::offset_of!(bkey_i, _data);

    fn nr(&self) -> usize {
        self.k.u64s as usize
    }
}

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
     *
     * Each type: its name and KEY_TYPE number, bch_bkey_type_flags, the
     * first field its value may end before - one added after the type was
     * created, which an older key hasn't - or _ if a valid value has them
     * all, and what it's for. The value up to that field, or all of it, is
     * the shortest a valid one is: its min_val_size. A value that's an array
     * - accounting's, damage's - checks it has an entry in its own validate.
     */
    BCH_BKEY_TYPES(x) {
        (deleted,               0,  0, _,
         "Transient during btree updates; inserting a deleted key "
         "removes any existing key at that position. "
         "Stripped during btree node writes."),
        (whiteout,              1,  0, _,
         "Blocks visibility of ancestor snapshot versions of a key"),
        (error,                 2,  0, err,
         "Marks an extent as containing unrecoverable errors"),
        (cookie,                3,  0, _,
         "Used by reconcile to track option changes via "
         "incrementing version numbers in the reconcile_scan btree"),
        (hash_whiteout,         4,  BKEY_TYPE_strict_btree_checks, _,
         "Whiteout for hash table btrees (dirents, xattrs) that "
         "preserves hash chain integrity"),
        (btree_ptr,             5,  BKEY_TYPE_strict_btree_checks, _,
         "Btree node pointer (v1, legacy)"),
        (extent,                6,  BKEY_TYPE_strict_btree_checks, _,
         "File data extent with device pointers, checksums, and "
         "optional compression metadata"),
        (reservation,           7,  BKEY_TYPE_strict_btree_checks, _,
         "Disk space reservation for a file region"),
        (inode,                 8,  BKEY_TYPE_strict_btree_checks, _,
         "Inode metadata (v1, legacy)"),
        (inode_generation,      9,  BKEY_TYPE_strict_btree_checks, _,
         "Tracks generation numbers for deleted inodes"),
        (dirent,                10, BKEY_TYPE_strict_btree_checks, _,
         "Directory entry with name hash, inode number, and type"),
        (xattr,                 11, BKEY_TYPE_strict_btree_checks, _,
         "Extended attribute with name hash and value"),
        (alloc,                 12, BKEY_TYPE_strict_btree_checks, _,
         "Bucket allocation metadata (v1, legacy)"),
        (quota,                 13, BKEY_TYPE_strict_btree_checks, _,
         "Quota counters for a user, group, or project"),
        (stripe,                14, BKEY_TYPE_strict_btree_checks, _,
         "Erasure code stripe metadata with parity pointers"),
        (reflink_p,             15, BKEY_TYPE_strict_btree_checks, _,
         "Pointer from the extents btree to an indirect extent "
         "in the reflink btree"),
        (reflink_v,             16, BKEY_TYPE_strict_btree_checks, _,
         "Indirect extent with refcount in the reflink btree"),
        (inline_data,           17, BKEY_TYPE_strict_btree_checks, _,
         "Small file data stored inline in the btree"),
        (btree_ptr_v2,          18, BKEY_TYPE_strict_btree_checks, _,
         "Btree node pointer (v2) with sequence number and "
         "min_key for more efficient traversal"),
        (indirect_inline_data,  19, BKEY_TYPE_strict_btree_checks, _,
         "Reflinked inline data with refcount"),
        (alloc_v2,              20, BKEY_TYPE_strict_btree_checks, _,
         "Bucket allocation metadata (v2, legacy)"),
        (subvolume,             21, BKEY_TYPE_strict_btree_checks, creation_parent,
         "Subvolume metadata with root inode and snapshot ID"),
        (snapshot,              22, BKEY_TYPE_strict_btree_checks, depth,
         "Snapshot tree node with parent, children, and "
         "subvolume references"),
        (inode_v2,              23, BKEY_TYPE_strict_btree_checks, _,
         "Inode metadata (v2) with journal sequence number"),
        (alloc_v3,              24, BKEY_TYPE_strict_btree_checks, _,
         "Bucket allocation metadata (v3, legacy)"),
        (set,                   25, 0, _,
         "Empty value; presence of the key is the information"),
        (lru,                   26, BKEY_TYPE_strict_btree_checks, _,
         "LRU tracking entry for cache eviction"),
        (alloc_v4,              27, BKEY_TYPE_strict_btree_checks, journal_seq_empty,
         "Bucket allocation metadata (v4, current)"),
        (backpointer,           28, BKEY_TYPE_strict_btree_checks, _,
         "Reverse pointer from a bucket back to the extent "
         "referencing it"),
        (inode_v3,              29, BKEY_TYPE_strict_btree_checks, _,
         "Inode metadata (v3, current) with compact encoding"),
        (bucket_gens,           30, BKEY_TYPE_strict_btree_checks, gens,
         "Packed bucket generation numbers (256 per key)"),
        (snapshot_tree,         31, BKEY_TYPE_strict_btree_checks, _,
         "Root entry for a snapshot tree structure"),
        (logged_op_truncate,    32, BKEY_TYPE_strict_btree_checks, _,
         "Logged truncate operation for crash recovery"),
        (logged_op_finsert,     33, BKEY_TYPE_strict_btree_checks, src_offset,
         "Logged file insert/collapse operation for crash recovery"),
        (accounting,            34, BKEY_TYPE_strict_btree_checks, _,
         "Disk accounting delta "
         "(replicas, compression, device usage)"),
        (inode_alloc_cursor,    35, BKEY_TYPE_strict_btree_checks, _,
         "Per-CPU inode number allocation cursor"),
        (extent_whiteout,       36, BKEY_TYPE_strict_btree_checks, _,
         "Whiteout specific to the extents btree, blocking "
         "visibility of ancestor snapshot extent versions"),
        (logged_op_stripe_update, 37, BKEY_TYPE_strict_btree_checks, _,
         "Logged stripe creation/update operation for crash recovery"),
        (damage,                38, BKEY_TYPE_strict_btree_checks, _,
         "Errors that damaged an inode, recorded in the same "
         "transaction as the repair that did the damage: a sorted "
         "list of bch_sb_error_id"),
        (logged_op_inode_opt_propagate, 39, BKEY_TYPE_strict_btree_checks, _,
         "Logged propagation of inode io options to ancestor "
         "snapshot versions"),
    }
}

c_enum! {
    #[open]
    pub enum bch_bkey_type: u32 {
        BCH_BKEY_TYPES(name, nr, ..) => KEY_TYPE_ ## name = nr,
        KEY_TYPE_MAX,
    }
}

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
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo, View)]
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
     * sb/errors_format.rs, name for name: the superblock counts how often a
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

c_enum! {
    #[open]
    pub enum bch_key_type_errors: u32 {
        KEY_TYPE_ERRORS(n, t) => KEY_TYPE_ERROR_ ## n = t,
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_cookie {
    pub v: c::bch_val,
    pub cookie: Le64,
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
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
#[c_packed]
pub struct bch_csum {
    pub lo: Le64,
    pub hi: Le64,
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

/// Its u64s count it from its start, its header's included: C's _data[] is
/// there, at 0, as vstructs.h's macros have it. The unsized tail is Rust's
/// alone, past the header - C has it as _data[] already.
///
/// The count is the crate's, not private: the sections that embed this
/// header are defined across the crate, and their vstruct!() reads it.
#[unsized_vstruct(tail = [u64])]
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bch_sb_field {
    pub _data: [u64; 0],
    pub(crate) u64s: Le32,
    pub type_: Le32,
}
vstruct!(unsized bch_sb_field, u64s, start = 0);

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

c_enum! {
    #[open]
    pub enum btree_id: u32 {
        BCH_BTREE_IDS(name, nr, ..) => BTREE_ID_ ## name = nr,
        BTREE_ID_NR,
    }
}

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

c_enum! {
    #[open]
    pub enum bch_sb_field_type: u32 {
        BCH_SB_FIELDS(f, nr, ..) => BCH_SB_FIELD_ ## f = nr,
        BCH_SB_FIELD_NR,
    }
}

/* A superblock field is one of its sections, by its type: */
tagged_union! {
    pub union bch_sb_field in c::bch_sb_field {
        tag type_: u32 = c::bch_sb_field_type,
        arms from BCH_SB_FIELDS(f, nr, ..) => f: c::bch_sb_field_ ## f = nr,
        unsized,
    }
}

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

#[unsized_vstruct]
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bch_sb_field_journal {
    pub field: Ffi<c::bch_sb_field>,
    pub buckets: [Le64; 0],
}
vstruct!(unsized bch_sb_field_journal, field.u64s, start = 0);

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bch_sb_field_journal_v2_entry {
    pub start: Le64,
    pub nr: Le64,
}

#[unsized_vstruct]
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bch_sb_field_journal_v2 {
    pub field: Ffi<c::bch_sb_field>,

    pub d: [c::bch_sb_field_journal_v2_entry; 0],
}
vstruct!(unsized bch_sb_field_journal_v2, field.u64s, start = 0);

/* BCH_SB_FIELD_crypt: */

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct nonce {
    pub d: [Le32; 4],
}

#[repr(C)]
#[derive(Default, CStruct, Plain, TypeInfo)]
pub struct bch_key {
    pub key: [Le64; 4],
}

#[repr(C)]
#[derive(Default, CStruct, Plain, TypeInfo)]
pub struct bch_encrypted_key {
    pub magic: Le64,
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
#[derive(Default, CStruct, Plain, TypeInfo)]
pub struct bch_sb_field_crypt {
    pub field: Ffi<c::bch_sb_field>,

    pub flags: Le64,
    pub kdf_flags: Le64,
    pub key: c::bch_encrypted_key,
}
vstruct!(bch_sb_field_crypt, field.u64s, start = 0);
whole_arm!(bch_sb_field_crypt);

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
#[unsized_vstruct]
#[repr(C)]
#[derive(Default, CStruct, Plain, TypeInfo)]
pub struct jset_entry {
    u64s: Le16,
    pub btree_id: u8,
    pub level: u8,
    pub type_: u8, /* designates what this jset holds */
    pub pad: [u8; 3],

    pub start: [c::bkey_i; 0],
    pub _data: [u64; 0],
}
vstruct!(unsized jset_entry, u64s);
vstruct_container!(jset_entry, thin c::bkey_i);

#[unsized_vstruct]
#[repr(C)]
#[derive(Default, CStruct, Plain, TypeInfo)]
pub struct bch_sb_field_clean {
    pub field: Ffi<c::bch_sb_field>,

    pub flags: Le32,
    pub _read_clock: Le16, /* no longer used */
    pub _write_clock: Le16,
    pub journal_seq: Le64,

    pub start: [Ffi<c::jset_entry>; 0],
    pub _data: [u64; 0],
}
vstruct!(unsized bch_sb_field_clean, field.u64s, start = 0);
vstruct_container!(bch_sb_field_clean, jset_entry);

/* sections from before btrees_lost_data are 88 bytes: what validate accepts */
#[grown(from = btrees_lost_data)]
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bch_sb_field_ext {
    pub field: Ffi<c::bch_sb_field>,
    pub recovery_passes_required: [Le64; 2],
    pub errors_silent: [Le64; 8],
    pub btrees_lost_data: Le64,
    pub flags0: Le64,
    /*
     * Like btrees_lost_data, but never cleared - btrees_lost_data gates
     * reconstruction and is cleared when repair completes; this is the
     * forensic record of every btree that has ever lost data:
     */
    pub btrees_lost_data_ever: Le64,
    /*
     * Btrees validated consistent by their check pass and not mutated
     * since. Written synchronously on every change (set on clean pass
     * completion, cleared from the btree's transactional trigger on
     * mutation) so the on-disk value is always current. Lets consistency
     * checks that would otherwise destroy data based on an in-memory table
     * they can't fully trust (check_key_has_snapshot) instead reschedule
     * the check pass. Runtime copy: bch_sb.btrees_clean.
     */
    pub btrees_clean: Le64,
}
vstruct!(bch_sb_field_ext__ffi, field.u64s, start = 0);

c_bitmask! {
    LE64_BITMASK(struct Ffi<bch_sb_field_ext>, flags0) {
        BCH_SB_EXT_DEV_READAHEAD(0, 20),
        BCH_SB_EXT_EC_STRIPE_BUF_LIMIT(20, 26),
        BCH_SB_EXT_SCRUB_MAX_REWIND_SECS(26, 38),
        BCH_SB_EXT_DISCARD_BUFFER(38, 42),
        BCH_SB_EXT_BTREE_CACHE_SHRINKER_SEEKS(42, 49),
        BCH_SB_EXT_MISSING_DEV_TIMEOUT(49, 61),
    }
}

// Superblock:

// New versioning scheme:
// One common version number for all on disk data structures - superblock, btree
// nodes, journal entries

#[allow(non_snake_case)]
pub const fn BCH_VERSION_MAJOR(v: u64) -> u64 { v >> 10 }

#[allow(non_snake_case)]
pub const fn BCH_VERSION_MINOR(v: u64) -> u64 { v & !(!0 << 10) }

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

c_enum! {
    #[open]
    pub enum bcachefs_metadata_version: u32 {
        bcachefs_metadata_version_min = 9,
        BCH_METADATA_VERSIONS(t, n, ..) => bcachefs_metadata_version_ ## t = n,
        bcachefs_metadata_version_max,
    }
}

c_const! {
    #[c_int]
    pub const bcachefs_metadata_version_current: u32 = c::bcachefs_metadata_version_max as u32 - 1;

    #[c_int]
    pub const BCH_SB_SECTOR: u32 = 8;

    #[c_int]
    pub const BCH_SB_LAYOUT_SIZE_BITS_MAX: u32 = 16; /* 32 MB */
}

#[repr(C, align(8))]
#[derive(Clone, Copy, CStruct, Plain, TypeInfo)]
#[c_packed]
pub struct bch_sb_layout {
    pub magic: c::__uuid_t, /* bcachefs superblock UUID */
    pub layout_type: u8,
    pub sb_max_size_bits: u8, /* base 2 of 512 byte sectors */
    pub nr_superblocks: u8,
    pub pad: [u8; 5],
    pub sb_offset: [Le64; 61],
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
#[unsized_vstruct]
#[repr(C, align(8))]
#[derive(Clone, Copy, CStruct, Plain, TypeInfo)]
#[c_packed]
pub struct bch_sb {
    pub csum: c::bch_csum,
    pub version: Le16,
    pub version_min: Le16,
    pub pad: [Le16; 2],
    pub magic: c::__uuid_t,
    pub uuid: c::__uuid_t,
    pub user_uuid: c::__uuid_t,
    pub label: [u8; c::BCH_SB_LABEL_SIZE as usize],
    pub offset: Le64,
    pub seq: Le64,

    pub block_size: Le16,
    pub dev_idx: u8,
    pub nr_devices: u8,
    u64s: Le32,

    pub time_base_lo: Le64,
    pub time_base_hi: Le32,
    pub time_precision: Le32,

    pub flags: [Le64; 7],
    pub write_time: Le64,
    pub features: [Le64; 2],
    pub compat: [Le64; 2],

    pub layout: c::bch_sb_layout,

    pub start: [Ffi<c::bch_sb_field>; 0],
    pub _data: [Le64; 0],
}
vstruct!(unsized bch_sb, u64s);
vstruct_container!(bch_sb, bch_sb_field);
c_default!(bch_sb__ffi);

c_bitmask! {
    LE16_BITMASK(struct Ffi<bch_sb>, block_size), strip BCH_SB_ {
        BCH_SB_BLOCK_SIZE(0, 16),
    }
}

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

c_bitmask! {
    LE64_BITMASK(struct Ffi<bch_sb>, flags[0]), strip BCH_SB_ {
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
    LE64_BITMASK(struct Ffi<bch_sb>, flags[1]), strip BCH_SB_ {
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
    LE64_BITMASK(struct Ffi<bch_sb>, flags[2]), strip BCH_SB_ {
        BCH_SB_BACKGROUND_COMPRESSION_TYPE_LO(0, 4),
        BCH_SB_GC_RESERVE_BYTES(4, 64),
    }
}

c_bitmask! {
    LE64_BITMASK(struct Ffi<bch_sb>, flags[3]), strip BCH_SB_ {
        BCH_SB_ERASURE_CODE(0, 16),
        BCH_SB_METADATA_TARGET(16, 28),
        BCH_SB_SHARD_INUMS(28, 29),
        BCH_SB_JOURNAL_FLUSH_DELAY(30, 62),
        BCH_SB_JOURNAL_FLUSH_DISABLED(62, 63),
        BCH_SB_MULTI_DEVICE(63, 64),
    }
}

c_bitmask! {
    LE64_BITMASK(struct Ffi<bch_sb>, flags[4]), strip BCH_SB_ {
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
    LE64_BITMASK(struct Ffi<bch_sb>, flags[5]), strip BCH_SB_ {
        BCH_SB_VERSION_UPGRADE_COMPLETE(0, 16),
        BCH_SB_ALLOCATOR_STUCK_TIMEOUT(16, 32),
        BCH_SB_VERSION_INCOMPAT(32, 48),
        BCH_SB_VERSION_INCOMPAT_ALLOWED(48, 64),
    }
}

c_bitmask! {
    LE64_BITMASK(struct Ffi<bch_sb>, flags[6]), strip BCH_SB_ {
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

    pub const BCH_SB_FEATURES_ALL: u64 =
        c::BCH_SB_FEATURES_ALWAYS |
        (1 << c::BCH_FEATURE_new_siphash as u32) |
        (1 << c::BCH_FEATURE_btree_ptr_v2 as u32) |
        (1 << c::BCH_FEATURE_new_varint as u32) |
        (1 << c::BCH_FEATURE_journal_no_flush as u32) |
        (1 << c::BCH_FEATURE_incompat_version_field as u32);
}

c_enum! {
    #[open]
    pub enum bch_sb_feature: u32 {
        BCH_SB_FEATURES(f, ..) => BCH_FEATURE_ ## f,
        BCH_FEATURE_NR,
    }
}

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

c_enum! {
    #[open]
    pub enum bch_sb_compat: u32 {
        BCH_SB_COMPAT(f, ..) => BCH_COMPAT_ ## f,
        BCH_COMPAT_NR,
    }
}

/* options: */

c_xmacro! {
    BCH_VERSION_UPGRADE_OPTS(x) {
        (compatible, 0),
        (incompatible, 1),
        (none, 2),
    }
}

c_enum! {
    #[open]
    pub enum bch_version_upgrade_opts: u32 {
        BCH_VERSION_UPGRADE_OPTS(t, n) => BCH_VERSION_UPGRADE_ ## t = n,
    }
}

c_const! {
    pub const BCH_REPLICAS_MAX: u32 = 4;

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

c_enum! {
    #[open]
    pub enum bch_error_actions: u32 {
        BCH_ERROR_ACTIONS(t, n, ..) => BCH_ON_ERROR_ ## t = n,
        BCH_ON_ERROR_NR,
    }
}

c_xmacro! {
    BCH_DEGRADED_ACTIONS(x) {
        (ask, 0),
        (yes, 1),
        (very, 2),
        (no, 3),
    }
}

c_enum! {
    #[open]
    pub enum bch_degraded_actions: u32 {
        BCH_DEGRADED_ACTIONS(t, n) => BCH_DEGRADED_ ## t = n,
        BCH_DEGRADED_ACTIONS_NR,
    }
}

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

c_enum! {
    #[open]
    pub enum bch_write_degraded_actions: u32 {
        BCH_WRITE_DEGRADED_ACTIONS(t, n) => BCH_WRITE_DEGRADED_ ## t = n,
        BCH_WRITE_DEGRADED_ACTIONS_NR,
    }
}

c_xmacro! {
    BCH_STR_HASH_TYPES(x) {
        (crc32c, 0),
        (crc64, 1),
        (siphash_old, 2),
        (siphash, 3),
    }
}

c_enum! {
    #[open]
    pub enum bch_str_hash_type: u32 {
        BCH_STR_HASH_TYPES(t, n) => BCH_STR_HASH_ ## t = n,
        BCH_STR_HASH_NR,
    }
}

c_xmacro! {
    BCH_STR_HASH_OPTS(x) {
        (crc32c, 0),
        (crc64, 1),
        (siphash, 2),
    }
}

c_enum! {
    #[open]
    pub enum bch_str_hash_opts: u32 {
        BCH_STR_HASH_OPTS(t, n) => BCH_STR_HASH_OPT_ ## t = n,
        BCH_STR_HASH_OPT_NR,
    }
}

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

c_enum! {
    #[open]
    pub enum bch_csum_type: u32 {
        BCH_CSUM_TYPES(t, n) => BCH_CSUM_ ## t = n,
        BCH_CSUM_NR,
    }
}

c_xmacro! {
    BCH_CSUM_OPTS(x) {
        (none, 0),
        (crc32c, 1),
        (crc64, 2),
        (xxhash, 3),
    }
}

c_enum! {
    #[open]
    pub enum bch_csum_opt: u32 {
        BCH_CSUM_OPTS(t, n) => BCH_CSUM_OPT_ ## t = n,
        BCH_CSUM_OPT_NR,
    }
}

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

c_enum! {
    #[open]
    pub enum bch_compression_type: u32 {
        BCH_COMPRESSION_TYPES(t, n) => BCH_COMPRESSION_TYPE_ ## t = n,
        BCH_COMPRESSION_TYPE_NR,
    }
}

c_xmacro! {
    BCH_COMPRESSION_OPTS(x) {
        (none, 0),
        (lz4, 1),
        (gzip, 2),
        (zstd, 3),
    }
}

c_enum! {
    #[open]
    pub enum bch_compression_opts: u32 {
        BCH_COMPRESSION_OPTS(t, n) => BCH_COMPRESSION_OPT_ ## t = n,
        BCH_COMPRESSION_OPT_NR,
    }
}

c_xmacro! {
    BCH_SCRUB_JOURNAL_OPTS(x) {
        (unclean, 0),
        (no, 1),
        (always, 2),
    }
}

c_enum! {
    #[open]
    pub enum bch_scrub_journal_opts: u32 {
        BCH_SCRUB_JOURNAL_OPTS(t, n) => BCH_SCRUB_JOURNAL_ ## t = n,
        BCH_SCRUB_JOURNAL_OPT_NR,
    }
}

c_const! {
    /* statfs()'s f_type, and the superblock's s_magic: the kernel's
     * uapi/linux/magic.h has it too, as BCACHEFS_SUPER_MAGIC */
    pub const BCACHEFS_STATFS_MAGIC: u32 = 0xca451a4e;

    /* Journal */
    pub const JSET_KEYS_U64s: usize = size_of::<Ffi<c::jset_entry>>() / size_of::<u64>();
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

c_enum! {
    #[open]
    pub enum bch_jset_entry_type: u32 {
        BCH_JSET_ENTRY_TYPES(f, nr, ..) => BCH_JSET_ENTRY_ ## f = nr,
        BCH_JSET_ENTRY_NR,
    }
}

/*
 * A journal entry is the type its type says. Six have no struct of their own:
 * keys, or a legacy prio_ptrs, in the entry itself - so the arms are written
 * out, not from BCH_JSET_ENTRY_TYPES, which would name one for each.
 */
tagged_union! {
    pub union jset_entry in c::jset_entry {
        tag type_: u8 = c::bch_jset_entry_type,
        arm btree_keys:        c::jset_entry              = c::bch_jset_entry_type::BCH_JSET_ENTRY_btree_keys.0,
        arm btree_root:        c::jset_entry              = c::bch_jset_entry_type::BCH_JSET_ENTRY_btree_root.0,
        arm prio_ptrs:         c::jset_entry              = c::bch_jset_entry_type::BCH_JSET_ENTRY_prio_ptrs.0,
        arm blacklist:         c::jset_entry_blacklist    = c::bch_jset_entry_type::BCH_JSET_ENTRY_blacklist.0,
        arm blacklist_v2:      c::jset_entry_blacklist_v2 = c::bch_jset_entry_type::BCH_JSET_ENTRY_blacklist_v2.0,
        arm usage:             c::jset_entry_usage        = c::bch_jset_entry_type::BCH_JSET_ENTRY_usage.0,
        arm data_usage:        c::jset_entry_data_usage   = c::bch_jset_entry_type::BCH_JSET_ENTRY_data_usage.0,
        arm clock:             c::jset_entry_clock        = c::bch_jset_entry_type::BCH_JSET_ENTRY_clock.0,
        arm dev_usage:         c::jset_entry_dev_usage    = c::bch_jset_entry_type::BCH_JSET_ENTRY_dev_usage.0,
        arm log:               c::jset_entry_log          = c::bch_jset_entry_type::BCH_JSET_ENTRY_log.0,
        arm overwrite:         c::jset_entry              = c::bch_jset_entry_type::BCH_JSET_ENTRY_overwrite.0,
        arm write_buffer_keys: c::jset_entry              = c::bch_jset_entry_type::BCH_JSET_ENTRY_write_buffer_keys.0,
        arm datetime:          c::jset_entry_datetime     = c::bch_jset_entry_type::BCH_JSET_ENTRY_datetime.0,
        arm log_bkey:          c::jset_entry              = c::bch_jset_entry_type::BCH_JSET_ENTRY_log_bkey.0,
        arm rewind_limit:      c::jset_entry_rewind_limit = c::bch_jset_entry_type::BCH_JSET_ENTRY_rewind_limit.0,
        arm rewind:            c::jset_entry_rewind       = c::bch_jset_entry_type::BCH_JSET_ENTRY_rewind.0,
        unsized,
    }
}

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
#[derive(Default, CStruct, Plain, TypeInfo)]
pub struct jset_entry_blacklist {
    pub entry: Ffi<c::jset_entry>,
    pub seq: Le64,
}
vstruct!(jset_entry_blacklist, entry.u64s);
whole_arm!(jset_entry_blacklist);

#[repr(C)]
#[derive(Default, CStruct, Plain, TypeInfo)]
pub struct jset_entry_blacklist_v2 {
    pub entry: Ffi<c::jset_entry>,
    pub start: Le64,
    pub end: Le64,
}
vstruct!(jset_entry_blacklist_v2, entry.u64s);
whole_arm!(jset_entry_blacklist_v2);

c_xmacro! {
    BCH_FS_USAGE_TYPES(x) {
        (reserved, 0),
        (inodes, 1),
        (key_version, 2),
    }
}

c_enum! {
    #[open]
    pub enum bch_fs_usage_type: u32 {
        BCH_FS_USAGE_TYPES(f, nr) => BCH_FS_USAGE_ ## f = nr,
        BCH_FS_USAGE_NR,
    }
}

#[repr(C, packed)]
#[derive(Default, CStruct, Plain, TypeInfo)]
pub struct jset_entry_usage {
    pub entry: Ffi<c::jset_entry>,
    pub v: Le64,
}
vstruct!(jset_entry_usage, entry.u64s);
whole_arm!(jset_entry_usage);

/* r's devs[] are in the tail: the entry's size is its u64s, not r's nr_devs */
#[unsized_vstruct(tail = [u8])]
#[repr(C, packed)]
#[derive(Default, CStruct, Plain, TypeInfo)]
pub struct jset_entry_data_usage {
    pub entry: Ffi<c::jset_entry>,
    pub v: Le64,
    pub r: c::bch_replicas_entry_v1,
}
vstruct!(unsized jset_entry_data_usage, entry.u64s);

#[repr(C, packed)]
#[derive(Default, CStruct, Plain, TypeInfo)]
pub struct jset_entry_clock {
    pub entry: Ffi<c::jset_entry>,
    pub rw: u8,
    pub pad: [u8; 7],
    pub time: Le64,
}
vstruct!(jset_entry_clock, entry.u64s);
whole_arm!(jset_entry_clock);

#[repr(C, packed)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct jset_entry_dev_usage_type {
    pub buckets: Le64,
    pub sectors: Le64,
    pub fragmented: Le64,
}

#[unsized_vstruct]
#[repr(C)]
#[derive(Default, CStruct, Plain, TypeInfo)]
pub struct jset_entry_dev_usage {
    pub entry: Ffi<c::jset_entry>,
    pub dev: Le32,
    pub pad: u32,

    pub _buckets_ec: Le64, /* No longer used */
    pub _buckets_unavailable: Le64, /* No longer used */

    pub d: [c::jset_entry_dev_usage_type; 0],
}
vstruct!(unsized jset_entry_dev_usage, entry.u64s);

#[unsized_vstruct]
#[repr(C, align(8))]
#[derive(Default, CStruct, Plain, TypeInfo)]
#[c_packed]
pub struct jset_entry_log {
    pub entry: Ffi<c::jset_entry>,
    pub d: [u8; 0],
}
vstruct!(unsized jset_entry_log, entry.u64s);

#[repr(C, align(8))]
#[derive(Default, CStruct, Plain, TypeInfo)]
#[c_packed]
pub struct jset_entry_datetime {
    pub entry: Ffi<c::jset_entry>,
    pub seconds: Le64,
}
vstruct!(jset_entry_datetime, entry.u64s);
whole_arm!(jset_entry_datetime);

#[repr(C, align(8))]
#[derive(Default, CStruct, Plain, TypeInfo)]
#[c_packed]
pub struct jset_entry_rewind_limit {
    pub entry: Ffi<c::jset_entry>,
    pub seq: Le64,
}
vstruct!(jset_entry_rewind_limit, entry.u64s);
whole_arm!(jset_entry_rewind_limit);

/*
 * Records a journal rewind in progress. Keys from journal entries
 * with seq in (to, from] use overwrite entries instead of btree_keys.
 */
#[repr(C, align(8))]
#[derive(Default, CStruct, Plain, TypeInfo)]
#[c_packed]
pub struct jset_entry_rewind {
    pub entry: Ffi<c::jset_entry>,
    pub from: Le64,
    pub to: Le64,
}
vstruct!(jset_entry_rewind, entry.u64s);
whole_arm!(jset_entry_rewind);

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
#[unsized_vstruct]
#[repr(C, align(8))]
#[derive(Default, CStruct, Plain, TypeInfo)]
#[c_packed]
pub struct jset {
    pub csum: c::bch_csum,

    pub magic: Le64,
    pub seq: Le64,
    pub version: Le32,
    pub flags: Le32,

    u64s: Le32, /* size of d[] in u64s */

    pub encrypted_start: [u8; 0],

    pub _read_clock: Le16, /* no longer used */
    pub _write_clock: Le16,

    /* Sequence number of oldest dirty journal entry */
    pub last_seq: Le64,

    pub start: [Ffi<c::jset_entry>; 0],
    pub _data: [u64; 0],
}
vstruct!(unsized jset, u64s);
vstruct_container!(jset, jset_entry);

/* A journal entry's keys, Views as its walk yields them: each one's header is whole. */
floored_by_self!(bkey_i);

c_bitmask! {
    LE32_BITMASK(struct Ffi<jset>, flags), strip JSET_ {
        JSET_CSUM_TYPE(0, 4),
        JSET_BIG_ENDIAN(4, 5),
        JSET_NO_FLUSH(5, 6),
        JSET_HAS_OVERWRITES(6, 7),
    }
}

c_const! {
    #[c_int]
    pub const BCH_JOURNAL_BUCKETS_MIN: u32 = 8;

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
#[unsized_vstruct]
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
#[c_packed]
pub struct bset {
    pub seq: Le64,

    /*
     * Highest journal entry this bset contains keys for.
     * If on recovery we don't see that journal entry, this bset is ignored:
     * this allows us to preserve the order of all index updates after a
     * crash, since the journal records a total order of all index updates
     * and anything that didn't make it to the journal doesn't get used.
     */
    pub journal_seq: Le64,

    pub flags: Le32,
    pub version: Le16,
    u64s: Le16, /* count of d[] in u64s */

    pub start: [c::bkey_packed; 0],
    pub _data: [u64; 0],
}
vstruct!(unsized bset, u64s);

c_bitmask! {
    LE32_BITMASK(struct Ffi<bset>, flags), strip BSET_ {
        BSET_CSUM_TYPE(0, 4),

        BSET_BIG_ENDIAN(4, 5),
        BSET_SEPARATE_WHITEOUTS(5, 6),

        /* Sector offset within the btree node: */
        BSET_OFFSET(16, 32),
    }
}

#[unsized_vstruct(tail = [u64])]
#[repr(C, align(8))]
#[derive(Clone, Copy, CStruct, Plain, TypeInfo)]
#[c_packed]
pub struct btree_node {
    pub csum: c::bch_csum,
    pub magic: Le64,

    /* this flags field is encrypted, unlike bset->flags: */
    pub flags: Le64,

    /* Closed interval: */
    pub min_key: c::bpos,
    pub max_key: c::bpos,
    pub _ptr: c::bch_extent_ptr, /* not used anymore */
    pub format: c::bkey_format,

    /* C's vstruct macros find u64s and _data on the node itself: */
    #[c_anon("union {
		struct bset keys;
		struct {
			u8 pad[22];
			__le16 u64s;
			u64 _data[];
		};
	}")]
    pub keys: Ffi<c::bset>,
}
vstruct!(unsized btree_node, keys.u64s);
c_default!(btree_node__ffi);

c_bitmask! {
    LE64_BITMASK(struct Ffi<btree_node>, flags), strip BTREE_NODE_ {
        BTREE_NODE_ID_LO(0, 4),
        BTREE_NODE_LEVEL(4, 8),
        BTREE_NODE_NEW_EXTENT_OVERWRITE(8, 9),
        BTREE_NODE_ID_HI(9, 25),
        /* 25-32 unused */
        BTREE_NODE_SEQ(32, 64),
    }
}

#[unsized_vstruct(tail = [u64])]
#[repr(C, align(8))]
#[derive(Clone, Copy, CStruct, Plain, TypeInfo)]
#[c_packed]
pub struct btree_node_entry {
    pub csum: c::bch_csum,

    /* C's vstruct macros find u64s and _data on the entry itself: */
    #[c_anon("union {
		struct bset keys;
		struct {
			u8 pad[22];
			__le16 u64s;
			u64 _data[];
		};
	}")]
    pub keys: Ffi<c::bset>,
}
vstruct!(unsized btree_node_entry, keys.u64s);
c_default!(btree_node_entry__ffi);

c_verbatim!(r#"
_Static_assert(offsetof(struct btree_node, u64s) == offsetof(struct btree_node, keys.u64s) &&
	       offsetof(struct btree_node, _data) == offsetof(struct btree_node, keys._data),
	       "btree_node: u64s and _data aren't its bset's");
_Static_assert(offsetof(struct btree_node_entry, u64s) == offsetof(struct btree_node_entry, keys.u64s) &&
	       offsetof(struct btree_node_entry, _data) == offsetof(struct btree_node_entry, keys._data),
	       "btree_node_entry: u64s and _data aren't its bset's");
"#);
