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
use crate::util::vstructs::{bytes_of_mut, cast_plain, cast_plain_mut, floored_by_self, from_bytes_mut, vstruct,
                            vstruct_container, Head, FlexArray, Unvalidated, VStructError, VstructMut, VstructRef};
use crate::btree::bkey::BkeyPHeader;
use core::mem::offset_of;
use cstruct_macros::{bitfield, c_bitmask, bch_enum, grown, tagged_union, unsized_vstruct, whole, CStruct, Plain, TypeInfo, SizedVal};

#[path = "alloc/accounting_format.rs"] pub mod accounting;
pub use accounting::{DataType, DataTypes};
#[path = "sb/members_format.rs"] pub mod members;
pub use members::{MemberInitialized, MemberState};

#[cfg(__KERNEL__)]
#[allow(non_camel_case_types)]
pub type __uuid_t = c::uuid_t;

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
pub struct Bpos {
    pub snapshot: u32,
    pub offset: u64,
    pub inode: u64,
}

#[cfg(target_endian = "big")]
#[repr(C, packed)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct Bpos {
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
#[derive(Clone, Copy, Default, PartialEq, Eq, CStruct, Plain, TypeInfo)]
pub struct Bversion {
    pub lo: u64,
    pub hi: u32,
}

#[cfg(target_endian = "big")]
#[repr(C, packed)]
#[derive(Clone, Copy, Default, PartialEq, Eq, CStruct, Plain, TypeInfo)]
pub struct Bversion {
    pub hi: u32,
    pub lo: u64,
}

impl Bversion {
    pub const MAX: Self = Self { hi: !0, lo: !0 };
}

/// hi, then lo: not derived, as little endian lays lo out first.
impl Ord for Bversion {
    fn cmp(&self, r: &Self) -> core::cmp::Ordering {
        let (l, r) = (*self, *r);
        (l.hi, l.lo).cmp(&(r.hi, r.lo))
    }
}

impl PartialOrd for Bversion {
    fn partial_cmp(&self, r: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(r))
    }
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
pub struct bkey {
    /* Size of combined key and value, in u64s */
    pub u64s: u8,

    /* Format of key (0 for format local to btree node) */
    #[c_bitfield]
    pub format_bits: bkey_format_bits,

    /* Type of the value */
    pub type_: u8,

    pub pad: [u8; 1],

    pub bversion: Bversion,
    pub size: u32, /* extent size, in sectors */
    pub p: Bpos,
}

#[cfg(target_endian = "big")]
#[repr(C, packed)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bkey {
    /* Size of combined key and value, in u64s */
    pub u64s: u8,

    /* Format of key (0 for format local to btree node) */
    #[c_bitfield]
    pub format_bits: bkey_format_bits,

    /* Type of the value */
    pub type_: u8,

    pub p: Bpos,
    pub size: u32, /* extent size, in sectors */
    pub bversion: Bversion,

    pub pad: [u8; 1],
}

#[repr(C, align(8))]
#[derive(Clone, Copy, CStruct, Plain, TypeInfo)]
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
    pub pad: [u8; core::mem::size_of::<c::bkey>() - 3],
}
vstruct!(bkey_packed, u64s, start = 0);
c_default!(bkey_packed);

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bch_le128 {
    pub lo: Le64,
    pub hi: Le64,
}

#[allow(non_upper_case_globals)]
pub const BKEY_U64s: usize = size_of::<c::bkey>() / size_of::<u64>();
#[allow(non_upper_case_globals)]
pub const BKEY_U64s_MAX: u8 = u8::MAX;
#[allow(non_upper_case_globals)]
pub const BKEY_VAL_U64s_MAX: usize = c::BKEY_U64s_MAX as usize - c::BKEY_U64s;

pub const KEY_PACKED_BITS_START: u32 = 24;

pub const KEY_FORMAT_LOCAL_BTREE: u32 = 0;
pub const KEY_FORMAT_CURRENT: u32 = 1;

#[bch_enum(closed)]
#[repr(u32)]
pub enum BkeyField {
    BKEY_FIELD_INODE,
    BKEY_FIELD_OFFSET,
    BKEY_FIELD_SNAPSHOT,
    BKEY_FIELD_SIZE,
    BKEY_FIELD_VERSION_HI,
    BKEY_FIELD_VERSION_LO,
    BKEY_NR_FIELDS,
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

#[bch_enum(flags)]
#[prefix(BKEY_TYPE_)]
#[repr(u32)]
pub enum BkeyTypeFlags {
    /*
     * Free to be used in more btrees than list it - deleted, whiteouts,
     * error, cookie, set: in one that doesn't, it's invalid at commit, not
     * on read
     */
    generic     = 1 << 0,
    /* Its value has pointers - extent entries: bch2_bkey_ptrs_c()'s types */
    ptrs        = 1 << 1,
    /* An extents btree may hold it - deleted, and what each #[holds] */
    extent      = 1 << 2,
    /* An indirect extent - refcounted, in the reflink btree: what a reflink_p points to */
    indirect    = 1 << 3,
    /*
     * Its pointers can be edited in place - its extent entries run to the
     * value's end, so entries can be inserted and dropped: not a stripe's,
     * whose checksums and block counts follow them
     */
    ptrs_mut    = 1 << 4,
    /* A btree node pointer: a key above the leaves, or a btree root's */
    btree_ptr   = 1 << 5,
    /* Counted in its inode's i_sectors */
    allocation  = 1 << 6,
    /* Data a file reads: pointers to it, the data inline, or a reflink_p to it */
    data        = 1 << 7,
    /* A file's data in the extents btree, not through a reflink_p */
    user_data   = 1 << 8,
    /* The data itself, inline in the value, direct or indirect */
    inline_data = 1 << 9,
}

#[bch_enum(open)]
#[prefix(KEY_TYPE_)]
#[nr]
#[arm_columns(flags: BkeyTypeFlags)]
#[repr(u32)]
pub enum BkeyType {
    /// Transient during btree updates; inserting a deleted key removes any
    /// existing key at that position. Stripped during btree node writes.
    #[flags(extent, generic)]
    deleted(c::Deleted) = 0,
    /// Blocks visibility of ancestor snapshot versions of a key
    #[flags(extent, generic)]
    whiteout(c::Whiteout) = 1,
    /// Marks an extent as containing unrecoverable errors: a read of it
    /// fails, as the data was lost with a failed device; a write over it
    /// replaces it
    #[flags(extent, allocation, generic)]
    error(c::Error) = 2,
    /// Used by reconcile to track option changes via incrementing version
    /// numbers in the reconcile_scan btree
    #[flags(extent, generic)]
    cookie(c::Cookie) = 3,
    /// Whiteout for hash table btrees (dirents, xattrs) that preserves hash
    /// chain integrity
    hash_whiteout(c::HashWhiteout) = 4,
    /// Btree node pointer (v1, legacy)
    #[flags(ptrs, ptrs_mut, btree_ptr)]
    btree_ptr(c::BtreePtr) = 5,
    /// File data extent with device pointers, checksums, and optional
    /// compression metadata
    #[flags(ptrs, ptrs_mut, extent, allocation, data, user_data)]
    extent(c::Extent) = 6,
    /// Disk space reservation for a file region
    #[flags(extent, allocation, user_data)]
    reservation(c::Reservation) = 7,
    /// Inode metadata (v1, legacy)
    inode(c::Inode) = 8,
    /// Tracks generation numbers for deleted inodes
    inode_generation(c::InodeGeneration) = 9,
    /// Directory entry with name hash, inode number, and type
    dirent(c::Dirent) = 10,
    /// Extended attribute with name hash and value
    xattr(c::Xattr) = 11,
    /// Bucket allocation metadata (v1, legacy)
    alloc(c::Alloc) = 12,
    /// Quota counters for a user, group, or project
    quota(c::Quota) = 13,
    /// Erasure code stripe metadata with parity pointers
    #[flags(ptrs)]
    stripe(c::Stripe) = 14,
    /// Pointer from the extents btree to an indirect extent in the reflink
    /// btree
    #[flags(extent, allocation, data)]
    reflink_p(c::ReflinkP) = 15,
    /// Indirect extent with refcount in the reflink btree
    #[flags(ptrs, ptrs_mut, extent, indirect, allocation, data)]
    reflink_v(c::ReflinkV) = 16,
    /// Small file data stored inline in the btree
    #[flags(extent, allocation, data, user_data, inline_data)]
    inline_data(c::InlineData) = 17,
    /// Btree node pointer (v2) with sequence number and min_key for more
    /// efficient traversal
    #[flags(ptrs, ptrs_mut, btree_ptr)]
    btree_ptr_v2(c::BtreePtrV2) = 18,
    /// Reflinked inline data with refcount
    #[flags(extent, indirect, allocation, data, inline_data)]
    indirect_inline_data(c::IndirectInlineData) = 19,
    /// Bucket allocation metadata (v2, legacy)
    alloc_v2(c::AllocV2) = 20,
    /// Subvolume metadata with root inode and snapshot ID
    subvolume(c::Subvolume) = 21,
    /// Snapshot tree node with parent, children, and subvolume references
    snapshot(c::Snapshot) = 22,
    /// Inode metadata (v2) with journal sequence number
    inode_v2(c::InodeV2) = 23,
    /// Bucket allocation metadata (v3, legacy)
    alloc_v3(c::AllocV3) = 24,
    /// Empty value; presence of the key is the information
    #[flags(extent, generic)]
    set(c::Set) = 25,
    /// LRU tracking entry for cache eviction
    lru(c::Lru) = 26,
    /// Bucket allocation metadata (v4, current)
    alloc_v4(c::AllocV4) = 27,
    /// Reverse pointer from a bucket back to the extent referencing it
    backpointer(c::Backpointer) = 28,
    /// Inode metadata (v3, current) with compact encoding
    inode_v3(c::InodeV3) = 29,
    /// Packed bucket generation numbers (256 per key)
    bucket_gens(c::BucketGens) = 30,
    /// Root entry for a snapshot tree structure
    snapshot_tree(c::SnapshotTree) = 31,
    /// Logged truncate operation for crash recovery
    logged_op_truncate(c::LoggedOpTruncate) = 32,
    /// Logged file insert/collapse operation for crash recovery
    logged_op_finsert(c::LoggedOpFinsert) = 33,
    /// Disk accounting delta (replicas, compression, device usage)
    accounting(c::Accounting) = 34,
    /// Per-CPU inode number allocation cursor
    inode_alloc_cursor(c::InodeAllocCursor) = 35,
    /// Whiteout specific to the extents btree, blocking visibility of
    /// ancestor snapshot extent versions
    #[flags(extent)]
    extent_whiteout(c::ExtentWhiteout) = 36,
    /// Logged stripe creation/update operation for crash recovery
    logged_op_stripe_update(c::LoggedOpStripeUpdate) = 37,
    /// Errors that damaged an inode, recorded in the same transaction as
    /// the repair that did the damage: a sorted list of SbErrorId
    damage(c::Damage) = 38,
    /// Logged propagation of inode io options to ancestor snapshot versions
    logged_op_inode_opt_propagate(c::LoggedOpInodeOptPropagate) = 39,
    /// Small file data stored inline in the btree, with its own checksum
    #[flags(extent, allocation, data, user_data, inline_data)]
    inline_data_v2(c::InlineDataV2) = 40,
    /// Reflinked inline data with refcount and its own checksum
    #[flags(extent, indirect, allocation, data, inline_data)]
    indirect_inline_data_v2(c::IndirectInlineDataV2) = 41,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo, SizedVal)]
pub struct Deleted {
    pub v: c::bch_val,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo, SizedVal)]
pub struct Whiteout {
    pub v: c::bch_val,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo, SizedVal)]
pub struct ExtentWhiteout {
    pub v: c::bch_val,
}

#[grown(from = err, bkey)]
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct Error {
    pub v: c::bch_val,
    pub err: u8,
    pub pad: [u8; 7],
}

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
#[bch_enum(open)]
#[prefix(KEY_TYPE_ERROR_)]
#[repr(u32)]
pub enum ErrorKeyReason {
    unknown = 0,
    device_removed = 1,
    double_allocation = 2,
    no_valid_pointers_repair = 3,
    decompress_exceeded_max_encoded_extent = 4,
    decompress_lz4_old = 5,
    decompress_lz4 = 6,
    decompress_gzip = 7,
    decompress_gzip_size_mismatch = 8,
    decompress_zstd_src_len_bad = 9,
    decompress_zstd_size_mismatch = 10,
    decompress_zstd_corruption_detected = 11,
    decompress_zstd_checksum_wrong = 12,
    decompress_zstd_prefix_unknown = 13,
    decompress_zstd_src_size_wrong = 14,
    decompress_zstd_dst_size_too_small = 15,
    decompress_zstd_memory_allocation = 16,
    decompress_zstd_unknown = 17,
    decompress_unknown = 18,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo, SizedVal)]
pub struct Cookie {
    pub v: c::bch_val,
    pub cookie: Le64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo, SizedVal)]
pub struct HashWhiteout {
    pub v: c::bch_val,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo, SizedVal)]
pub struct Set {
    pub v: c::bch_val,
}

/* 128 bits, sufficient for cryptographic MACs: */
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, PartialEq, Eq, CStruct, Plain, TypeInfo)]
pub struct bch_csum {
    pub lo: Le64,
    pub hi: Le64,
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo, SizedVal)]
pub struct Backpointer {
    pub v: c::bch_val,
    pub btree_id: u8,
    pub level: u8,
    pub data_type: u8,
    pub bucket_gen: u8,
    pub flags: u32,
    pub bucket_len: u32,
    pub pos: Bpos,
}

c_bitmask! {
    BITMASK(struct Backpointer, flags), strip BACKPOINTER_ {
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

#[bch_enum(flags)]
#[prefix(BTREE_IS_)]
#[repr(u32)]
pub enum BtreeFlags {
    extents = 1 << 0,
    snapshots = 1 << 1,
    snapshot_field = 1 << 2,
    data = 1 << 3,
    write_buffer = 1 << 4,
    trans_triggers = 1 << 5,
    atomic_triggers = 1 << 6,
}

#[bch_enum(open)]
#[own_to_text]
#[prefix(BTREE_ID_)]
#[nr]
#[arm_columns(flags: BtreeFlags, holds: set(BkeyType))]
#[repr(u32)]
pub enum Btree {
    /// File data extent pointers and reservations
    #[flags(extents, snapshots, data, trans_triggers)]
    #[holds(whiteout, extent_whiteout, error, cookie, extent, reservation, reflink_p,
            inline_data, inline_data_v2)]
    extents = 0,
    /// Inode metadata
    #[flags(snapshots, trans_triggers, atomic_triggers)]
    #[holds(whiteout, inode, inode_v2, inode_v3, inode_generation)]
    inodes = 1,
    /// Directory entries
    #[flags(snapshots)]
    #[holds(whiteout, hash_whiteout, dirent)]
    dirents = 2,
    /// Extended attributes
    #[flags(snapshots)]
    #[holds(whiteout, cookie, hash_whiteout, xattr)]
    xattrs = 3,
    /// Bucket allocation state
    #[flags(trans_triggers, atomic_triggers)]
    #[holds(alloc, alloc_v2, alloc_v3, alloc_v4)]
    alloc = 4,
    /// User, group, and project quota counters
    #[holds(quota)]
    quotas = 5,
    /// Erasure coding stripe descriptors
    #[flags(data, trans_triggers, atomic_triggers)]
    #[holds(stripe)]
    stripes = 6,
    /// Reflink shared extent pointers
    #[flags(extents, data, trans_triggers)]
    #[holds(reflink_v, indirect_inline_data, indirect_inline_data_v2, error)]
    reflink = 7,
    /// Subvolume metadata
    #[flags(trans_triggers)]
    #[holds(subvolume)]
    subvolumes = 8,
    /// Snapshot tree structure
    #[flags(trans_triggers, atomic_triggers)]
    #[holds(snapshot)]
    snapshots = 9,
    /// Least-recently-used tracking for cache eviction
    #[flags(write_buffer)]
    #[holds(set)]
    lru = 10,
    /// Free space index
    #[flags(extents)]
    #[holds(set)]
    freespace = 11,
    /// Buckets waiting for discard/TRIM
    #[flags(write_buffer)]
    #[holds(set)]
    need_discard = 12,
    /// Reverse pointers from data extents back to btree nodes
    #[flags(snapshot_field, write_buffer)]
    #[holds(backpointer)]
    backpointers = 13,
    /// Bucket generation numbers for stale pointer detection
    #[holds(bucket_gens)]
    bucket_gens = 14,
    /// Snapshot tree roots
    #[holds(snapshot_tree)]
    snapshot_trees = 15,
    /// Inodes pending deletion
    #[flags(snapshot_field, write_buffer)]
    #[holds(set)]
    deleted_inodes = 16,
    /// In-progress logged operations for crash recovery
    #[holds(logged_op_truncate, logged_op_finsert, logged_op_stripe_update,
            logged_op_inode_opt_propagate, inode_alloc_cursor)]
    logged_ops = 17,
    /// Reconcile work items
    #[flags(snapshot_field, write_buffer)]
    #[holds(set, cookie)]
    reconcile_work = 18,
    /// Subvolume parent-child relationships
    #[holds(set)]
    subvolume_children = 19,
    /// Space accounting by replicas and disk groups
    #[flags(snapshot_field, write_buffer)]
    #[holds(accounting)]
    accounting = 20,
    /// High-priority reconcile work items
    #[flags(snapshot_field, write_buffer)]
    #[holds(set)]
    reconcile_hipri = 21,
    /// Pending reconcile work items
    #[flags(snapshot_field, write_buffer)]
    #[holds(set)]
    reconcile_pending = 22,
    /// Reconcile scan state
    #[holds(cookie, backpointer)]
    reconcile_scan = 23,
    /// Physical reconcile work items
    #[flags(snapshot_field, write_buffer)]
    #[holds(set)]
    reconcile_work_phys = 24,
    /// Physical high-priority reconcile work items
    #[flags(snapshot_field, write_buffer)]
    #[holds(set)]
    reconcile_hipri_phys = 25,
    /// Bucket to stripe mapping for erasure coding
    #[holds(set)]
    bucket_to_stripe = 26,
    /// Stripe backpointers
    #[flags(write_buffer)]
    #[holds(backpointer)]
    stripe_backpointers = 27,
    /// Inodes damaged by errors and repairs
    #[flags(snapshots)]
    #[holds(whiteout, damage)]
    damage = 28,
}

impl Btree {
    /// Its name: None for a btree this version doesn't know.
    pub fn name(self) -> Option<&'static str> {
        crate::util::names::name_of(<Self as crate::util::names::Names>::ENTRIES, self.0.into())
    }
}

/// A btree this version doesn't know is still said to be one, as C's
/// bch2_btree_id_to_text() has it: recovery meets them.
impl crate::util::ToText for Btree {
    fn to_text(&self, out: &mut crate::util::Printbuf) {
        match self.name() {
            Some(name) => crate::prt!(out, "{name}"),
            None       => crate::prt!(out, "(unknown btree {})", self.0),
        }
    }
}
crate::display_via_to_text!(Btree);

/* A superblock field is one of its sections, by its type: */
tagged_union! {
    #[storage(c::bch_sb_field)]
    #[tag(type_: u32)]
    #[tag_enum(SbFieldType: u32, prefix = BCH_SB_FIELD_, nr)]
    #[size(header)]
    pub enum bch_sb_field {
        /// Journal bucket list for this device
        journal(c::bch_sb_field_journal) = 0,
        /// Member device list (v1)
        members_v1(c::bch_sb_field_members_v1) = 1,
        /// Encryption key and KDF settings
        crypt(c::bch_sb_field_crypt) = 2,
        /// Replica entries (v0)
        replicas_v0(c::bch_sb_field_replicas_v0) = 3,
        /// Quota timelimit and warnlimit fields
        quota(c::bch_sb_field_quota) = 4,
        /// Device label strings and label path tree structure
        disk_groups(c::bch_sb_field_disk_groups) = 5,
        /// Clean shutdown: btree roots and usage counters, allowing journal
        /// replay to be skipped
        clean(c::bch_sb_field_clean) = 6,
        /// Replica entries: lists of devices with extents replicated across
        /// them
        replicas(c::bch_sb_field_replicas) = 7,
        /// Blacklisted journal sequence numbers
        journal_seq_blacklist(c::bch_sb_field_journal_seq_blacklist) = 8,
        /// Journal bucket list (v2)
        journal_v2(c::bch_sb_field_journal_v2) = 9,
        /// Persistent counters: IO statistics, error counts, and cumulative
        /// metrics across mounts
        counters(c::bch_sb_field_counters) = 10,
        /// Member device list (v2)
        members_v2(c::bch_sb_field_members_v2) = 11,
        /// Persistent error log: records errors detected during operation or
        /// fsck so they survive across mounts
        errors(c::bch_sb_field_errors) = 12,
        /// Extended superblock: required recovery passes, accumulated silenced
        /// errors, and other flags
        ext(c::bch_sb_field_ext) = 13,
        /// Minimum on-disk format version for safe downgrade, with required
        /// recovery passes
        downgrade(c::bch_sb_field_downgrade) = 14,
        /// Tracks which recovery passes have been run successfully
        recovery_passes(c::bch_sb_field_recovery_passes) = 15,
        /// Per-extent-type size limits
        extent_type_u64s(c::bch_sb_field_extent_type_u64s) = 16,
        /// Persistent error log, v2: adds the time of first occurrence to each
        /// entry
        errors_v2(c::bch_sb_field_errors_v2) = 17,
    }
}

/*
 * Most superblock fields are replicated in all device's superblocks - a few are
 * not:
 */
pub const BCH_SINGLE_DEVICE_SB_FIELDS: u32 =
    (1 << c::SbFieldType::journal.0) |
    (1 << c::SbFieldType::journal_v2.0);

/* BCH_SB_FIELD_journal: */

#[unsized_vstruct]
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bch_sb_field_journal {
    pub field: Head<c::bch_sb_field>,
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
    pub field: Head<c::bch_sb_field>,

    pub d: [c::bch_sb_field_journal_v2_entry; 0],
}
vstruct!(unsized bch_sb_field_journal_v2, field.u64s, start = 0);

/* BCH_SB_FIELD_crypt: */

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct Nonce {
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
#[whole]
#[repr(C)]
#[derive(Default, CStruct, Plain, TypeInfo)]
pub struct bch_sb_field_crypt {
    pub field: Head<c::bch_sb_field>,

    pub flags: Le64,
    pub kdf_flags: Le64,
    pub key: c::bch_encrypted_key,
}
vstruct!(bch_sb_field_crypt, field.u64s, start = 0);

c_bitmask! {
    LE64_BITMASK(struct bch_sb_field_crypt, flags) {
        BCH_CRYPT_KDF_TYPE(0, 4),
    }
}

#[bch_enum(open)]
#[repr(u32)]
pub enum KdfType {
    BCH_KDF_SCRYPT = 0,
    BCH_KDF_NR = 1,
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
    pub field: Head<c::bch_sb_field>,

    pub flags: Le32,
    pub _read_clock: Le16, /* no longer used */
    pub _write_clock: Le16,
    pub journal_seq: Le64,

    pub start: [Head<c::jset_entry>; 0],
    pub _data: [u64; 0],
}
vstruct!(unsized bch_sb_field_clean, field.u64s, start = 0);
vstruct_container!(bch_sb_field_clean, jset_entry);

/* sections from before btrees_lost_data are 88 bytes: what validate accepts */
#[grown(from = btrees_lost_data)]
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bch_sb_field_ext {
    pub field: Head<c::bch_sb_field>,
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
vstruct!(bch_sb_field_ext, field.u64s, start = 0);

c_bitmask! {
    LE64_BITMASK(struct bch_sb_field_ext, flags0), strip BCH_SB_EXT_ {
        BCH_SB_EXT_DEV_READAHEAD(0, 20),
        BCH_SB_EXT_EC_STRIPE_BUF_LIMIT(20, 26),
        BCH_SB_EXT_SCRUB_MAX_REWIND_SECS(26, 38),
        BCH_SB_EXT_DISCARD_BUFFER(38, 42),
        BCH_SB_EXT_BTREE_CACHE_SHRINKER_SEEKS(42, 49),
        BCH_SB_EXT_MISSING_DEV_TIMEOUT(49, 61),
        BCH_SB_EXT_JOURNAL_REPLICAS(61, 64),
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
pub const fn BCH_VERSION(major: u64, minor: u64) -> u64 { (major << 10) | minor }

#[bch_enum(open)]
#[own_to_text]
#[prefix(bcachefs_metadata_version_)]
#[nr]
#[repr(u32)]
pub enum MetadataVersion {
    /// 2018-11: Renumbered bkey type values
    bkey_renumber = BCH_VERSION(0, 10),
    /// 2020-03: Swapped inode/offset fields in bpos for better locality
    inode_btree_change = BCH_VERSION(0, 11),
    /// 2021-03: Snapshot support: snapshot field in bpos,
    /// KEY_TYPE_snapshot, KEY_TYPE_subvolume
    snapshot = BCH_VERSION(0, 12),
    /// 2021-04: Backpointers from extents to inodes
    inode_backpointers = BCH_VERSION(0, 13),
    /// 2021-07: sectors_written field in btree_ptr_v2 for partial write
    /// tracking
    btree_ptr_sectors_written = BCH_VERSION(0, 14),
    /// 2021-09: Snapshot fixes: subvolume root inode tracking
    snapshot_2 = BCH_VERSION(0, 15),
    /// 2021-10: Fixed reflink pointer refcount handling
    reflink_p_fix = BCH_VERSION(0, 16),
    /// 2021-10: Linked subvolumes to directory entries
    subvol_dirent = BCH_VERSION(0, 17),
    /// 2021-11: KEY_TYPE_inode_v2 with improved field layout
    inode_v2 = BCH_VERSION(0, 18),
    /// 2022-03: Freespace btree for faster allocation
    freespace = BCH_VERSION(0, 19),
    /// 2022-03: KEY_TYPE_alloc_v4 with comprehensive allocation metadata
    alloc_v4 = BCH_VERSION(0, 20),
    /// 2022-04: Refined data type classifications in the allocator
    new_data_types = BCH_VERSION(0, 21),
    /// 2022-06: Backpointers btree mapping physical locations back to
    /// logical extents
    backpointers = BCH_VERSION(0, 22),
    /// 2022-10: KEY_TYPE_inode_v3 with compact encoding
    inode_v3 = BCH_VERSION(0, 23),
    /// 2022-11: Preallocated extents that read as zeros
    unwritten_extents = BCH_VERSION(0, 24),
    /// 2022-12: bucket_gens btree for stale pointer detection
    bucket_gens = BCH_VERSION(0, 25),
    /// 2022-12: Improved LRU tracking for cache eviction
    lru_v2 = BCH_VERSION(0, 26),
    /// 2023-02: LRU tracking for fragmented extents
    fragmentation_lru = BCH_VERSION(0, 27),
    /// 2023-03: Backpointers moved fully to backpointers btree
    no_bps_in_alloc_keys = BCH_VERSION(0, 28),
    /// 2023-05: snapshot_trees btree for hierarchical management
    snapshot_trees = BCH_VERSION(0, 29),
    /// 2023-07: Major version bump: format stabilization, major.minor
    /// versioning scheme
    major_minor = BCH_VERSION(1,  0),
    /// 2023-07: Skiplist structure for O(log n) ancestor queries
    snapshot_skiplists = BCH_VERSION(1,  1),
    /// 2023-08: deleted_inodes btree for crash-safe unlink
    deleted_inodes = BCH_VERSION(1,  2),
    /// 2023-10: Background rebalance work tracking
    rebalance_work = BCH_VERSION(1,  3),
    /// 2023-12: Sequence numbers on member devices for stale superblock
    /// detection
    member_seq = BCH_VERSION(1,  4),
    /// 2024-02: fs_parent field on subvolumes for path resolution
    subvolume_fs_parent = BCH_VERSION(1,  5),
    /// 2024-02: subvolume_children btree for parent-child lookups
    btree_subvolume_children = BCH_VERSION(1,  6),
    /// 2024-04: Per-device bitmap tracking which btrees have data
    mi_btree_bitmap = BCH_VERSION(1,  7),
    /// 2024-06: Stripe sector tracking in alloc keys for erasure coding
    bucket_stripe_sectors = BCH_VERSION(1,  8),
    /// 2024-01: On-disk accounting btree for persistent space accounting
    disk_accounting_v2 = BCH_VERSION(1,  9),
    /// 2024-08: Accounting key validation and endianness fixes
    disk_accounting_v3 = BCH_VERSION(1, 10),
    /// 2024-08: Per-inode space accounting
    disk_accounting_inum = BCH_VERSION(1, 11),
    /// 2024-08: Fixed rebalance work accounting bugs
    rebalance_work_acct_fix = BCH_VERSION(1, 12),
    /// 2024-10: Flag on inodes indicating child snapshot data exists
    inode_has_child_snapshots = BCH_VERSION(1, 13),
    /// 2024-11: Bucket generation in backpointers for stale detection
    /// without alloc key reads
    backpointer_bucket_gen = BCH_VERSION(1, 14),
    /// 2024-11: Big-endian disk accounting fix
    disk_accounting_big_endian = BCH_VERSION(1, 15),
    /// 2024-12: reflink_p keys carry independently updatable extent options
    reflink_p_may_update_opts = BCH_VERSION(1, 16),
    /// 2024-12: Directory depth tracking in inodes
    inode_depth = BCH_VERSION(1, 17),
    /// 2024-12: Persistent inode number allocation cursors
    persistent_inode_cursors = BCH_VERSION(1, 18),
    /// 2024-12: Default error action changed to fix_safe
    autofix_errors = BCH_VERSION(1, 19),
    /// 2025-01: Directory size tracking in inode metadata
    directory_size = BCH_VERSION(1, 20),
    /// 2025-02: In-memory backpointer caching for faster lookups
    cached_backpointers = BCH_VERSION(1, 21),
    /// 2025-02: Backpointers for erasure-coded stripes
    stripe_backpointers = BCH_VERSION(1, 22),
    /// 2025-02: LRU tracking for stripe cache eviction
    stripe_lru = BCH_VERSION(1, 23),
    /// 2025-02: Case-insensitive filename support with per-directory flags
    casefolding = BCH_VERSION(1, 24),
    /// 2025-03: Flags field on extent keys for extent-level metadata
    extent_flags = BCH_VERSION(1, 25),
    /// 2025-05: Improved snapshot deletion with better interior node
    /// handling
    snapshot_deletion_v2 = BCH_VERSION(1, 26),
    /// 2025-05: Fast device removal when no stale pointers exist
    fast_device_removal = BCH_VERSION(1, 27),
    /// 2025-05: Casefold flag on inodes for directory handling
    inode_has_case_insensitive = BCH_VERSION(1, 28),
    /// 2025-08: KEY_TYPE_extent_whiteout for efficient snapshot extent
    /// deletion
    extent_snapshot_whiteouts = BCH_VERSION(1, 29),
    /// 2025-08: Extended directory entry offset to 31 bits
    bcachefs_metadata_version_31bit_dirent_offset = BCH_VERSION(1, 30),
    /// 2025-09: Per-btree-node space accounting
    btree_node_accounting = BCH_VERSION(1, 31),
    /// 2025-11: Superblock field for extent type size limits
    sb_field_extent_type_u64s = BCH_VERSION(1, 32),
    /// 2025-11: Reconcile system for IO options inheritance and background
    /// data movement
    reconcile = BCH_VERSION(1, 33),
    /// 2025-12: KEY_TYPE_error changed to zero-byte value
    extented_key_type_error = BCH_VERSION(1, 34),
    /// 2026-01: Stripe index in alloc keys for efficient stripe-bucket
    /// lookups
    bucket_stripe_index = BCH_VERSION(1, 35),
    /// 2026-01: Removed redundant user_data replicas from superblock
    no_sb_user_data_replicas = BCH_VERSION(1, 36),
    /// 2026-03: First release with fully supported erasure coding: all key
    /// functionality done, resilver integrated with reconcile
    erasure_coding = BCH_VERSION(1, 37),
    /// 2026-03: need_discard btree reindexed by journal seq for O(1)
    /// discard eligibility checks
    need_discard_by_journal_seq = BCH_VERSION(1, 38),
    /// 2026-07: Per-device bucket fragmentation LRUs, so copygc can reason
    /// about fragmentation per device
    per_dev_fragmentation_lru = BCH_VERSION(1, 39),
    /// 2026-11: Inline data extents carry their own checksums; stripe
    /// backpointers carry the stripe index, so stripes sharing a block
    /// during reuse don't share a backpointer
    inline_data_csum = BCH_VERSION(1, 40),
}

/* The oldest version this one reads */
#[allow(non_upper_case_globals)]
pub const bcachefs_metadata_version_min: u32 = c::MetadataVersion::btree_ptr_sectors_written.0;

#[allow(non_upper_case_globals)]
pub const bcachefs_metadata_version_current: u32 = c::MetadataVersion::NR as u32 - 1;

pub const BCH_SB_SECTOR: u32 = 8;

pub const BCH_SB_LAYOUT_SIZE_BITS_MAX: u32 = 16; /* 32 MB */

#[repr(C, align(8))]
#[derive(Clone, Copy, CStruct, Plain, TypeInfo)]
pub struct bch_sb_layout {
    pub magic: c::__uuid_t, /* bcachefs superblock UUID */
    pub layout_type: u8,
    pub sb_max_size_bits: u8, /* base 2 of 512 byte sectors */
    pub nr_superblocks: u8,
    pub pad: [u8; 5],
    pub sb_offset: [Le64; 61],
}
c_default!(bch_sb_layout);
// bch_dev's copy is read without sb_lock - by the allocator, until the
// superblock buckets are marked - and may be read mid-edit, each offset as
// before or after
unsafe impl crate::util::projections::TearSafe for bch_sb_layout {}

pub const BCH_SB_LAYOUT_SECTOR: u32 = 7;

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

    pub start: [Head<c::bch_sb_field>; 0],
    pub _data: [Le64; 0],
}
vstruct!(unsized bch_sb, u64s);
vstruct_container!(bch_sb, bch_sb_field);
c_default!(bch_sb__head);

impl bch_sb<Unvalidated> {
    /// Its count less @u64s, for a section shrunk in place, the sections after
    /// it moved down: sb/io.rs's, as validate repairs one.
    pub(crate) fn sub_u64s(&mut self, u64s: u32) {
        let h = cast_plain_mut::<bch_sb__head>(bytes_of_mut(self)).expect("a superblock's head");
        h.u64s = Le32::new(h.u64s.get() - u64s);
    }
}

c_bitmask! {
    LE16_BITMASK(struct Head<bch_sb>, block_size), strip BCH_SB_ {
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
    LE64_BITMASK(struct Head<bch_sb>, flags[0]), strip BCH_SB_ {
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
    LE64_BITMASK(struct Head<bch_sb>, flags[1]), strip BCH_SB_ {
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
    LE64_BITMASK(struct Head<bch_sb>, flags[2]), strip BCH_SB_ {
        BCH_SB_BACKGROUND_COMPRESSION_TYPE_LO(0, 4),
        BCH_SB_GC_RESERVE_BYTES(4, 64),
    }
}

c_bitmask! {
    LE64_BITMASK(struct Head<bch_sb>, flags[3]), strip BCH_SB_ {
        BCH_SB_ERASURE_CODE(0, 16),
        BCH_SB_METADATA_TARGET(16, 28),
        BCH_SB_SHARD_INUMS(28, 29),
        BCH_SB_JOURNAL_FLUSH_DELAY(30, 62),
        BCH_SB_JOURNAL_FLUSH_DISABLED(62, 63),
        BCH_SB_MULTI_DEVICE(63, 64),
    }
}

c_bitmask! {
    LE64_BITMASK(struct Head<bch_sb>, flags[4]), strip BCH_SB_ {
        BCH_SB_JOURNAL_RECLAIM_DELAY(0, 32),
        BCH_SB_JOURNAL_TRANSACTION_NAMES(32, 33),
        BCH_SB_NOCOW(33, 34),
        BCH_SB_WRITE_BUFFER_SIZE(34, 54),
        BCH_SB_VERSION_UPGRADE(54, 56),

        BCH_SB_COMPRESSION_TYPE_HI(56, 60),
        BCH_SB_BACKGROUND_COMPRESSION_TYPE_HI(60, 64),
    }
}

/// The compression options, each in two fields: _HI is the bits it grew -
/// The compression type, split across two fields.
impl bch_sb__head {
    pub fn compression_type(&self) -> u64 {
        self.compression_type_lo() | self.compression_type_hi() << 4
    }

    pub fn background_compression_type(&self) -> u64 {
        self.background_compression_type_lo() | self.background_compression_type_hi() << 4
    }

    pub fn set_compression_type(&mut self, v: u64) {
        self.set_compression_type_lo(v);
        self.set_compression_type_hi(v >> 4);
    }

    pub fn set_background_compression_type(&mut self, v: u64) {
        self.set_background_compression_type_lo(v);
        self.set_background_compression_type_hi(v >> 4);
    }
}

c_bitmask! {
    LE64_BITMASK(struct Head<bch_sb>, flags[5]), strip BCH_SB_ {
        BCH_SB_VERSION_UPGRADE_COMPLETE(0, 16),
        BCH_SB_ALLOCATOR_STUCK_TIMEOUT(16, 32),
        BCH_SB_VERSION_INCOMPAT(32, 48),
        BCH_SB_VERSION_INCOMPAT_ALLOWED(48, 64),
    }
}

c_bitmask! {
    LE64_BITMASK(struct Head<bch_sb>, flags[6]), strip BCH_SB_ {
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

pub const BCH_SB_EXTENT_BP_SHIFT_DEFAULT: u32 = 10;


pub const BCH_SB_FEATURES_ALWAYS: u64 =
    (1 << c::SbFeature::new_extent_overwrite.0) |
    (1 << c::SbFeature::extents_above_btree_updates.0) |
    (1 << c::SbFeature::btree_updates_journalled.0) |
    (1 << c::SbFeature::alloc_v2.0) |
    (1 << c::SbFeature::extents_across_btree_nodes.0);

pub const BCH_SB_FEATURES_ALL: u64 =
    c::BCH_SB_FEATURES_ALWAYS |
    (1 << c::SbFeature::new_siphash.0) |
    (1 << c::SbFeature::btree_ptr_v2.0) |
    (1 << c::SbFeature::new_varint.0) |
    (1 << c::SbFeature::journal_no_flush.0) |
    (1 << c::SbFeature::incompat_version_field.0);

#[bch_enum(open)]
#[prefix(BCH_FEATURE_)]
#[nr]
#[repr(u32)]
pub enum SbFeature {
    lz4 = 0,
    gzip = 1,
    zstd = 2,
    atomic_nlink = 3,
    ec = 4,
    /// Gates BCH_SB_FIELD_journal_seq_blacklist
    journal_seq_blacklist_v3 = 5,
    /// Gates KEY_TYPE_reflink
    reflink = 6,
    /// Gates BCH_STR_HASH_siphash
    new_siphash = 7,
    /// Gates KEY_TYPE_inline_data
    inline_data = 8,
    /// Gates BTREE_NODE_NEW_EXTENT_OVERWRITE
    new_extent_overwrite = 9,
    incompressible = 10,
    btree_ptr_v2 = 11,
    extents_above_btree_updates = 12,
    btree_updates_journalled = 13,
    reflink_inline_data = 14,
    new_varint = 15,
    journal_no_flush = 16,
    alloc_v2 = 17,
    extents_across_btree_nodes = 18,
    incompat_version_field = 19,
    casefolding = 20,
    no_alloc_info = 21,
    small_image = 22,
    no_default_sb = 23,
}

#[bch_enum(open)]
#[prefix(BCH_COMPAT_)]
#[nr]
#[repr(u32)]
pub enum SbCompat {
    alloc_info = 0,
    alloc_metadata = 1,
    extents_above_btree_updates_done = 2,
    bformat_overflow_done = 3,
    no_stale_ptrs = 4,
    stripe_frag_accounting = 5,
    inode_opts_propagated = 6,
}

/* options: */

#[bch_enum(open)]
#[prefix(BCH_VERSION_UPGRADE_)]
#[repr(u32)]
pub enum VersionUpgradeOpt {
    compatible = 0,
    incompatible = 1,
    none = 2,
}

pub const BCH_REPLICAS_MAX: u32 = 4;

pub const BCH_BKEY_PTRS_MAX: u32 = 16;

#[bch_enum(open)]
#[prefix(BCH_ON_ERROR_)]
#[nr]
#[repr(u32)]
pub enum ErrorAction {
    /// Log the error but continue normal operation without attempting
    /// repair
    BCH_ON_ERROR_continue = 0,
    /// Automatically repair errors that are safe to fix without user
    /// confirmation. Unsafe errors cause emergency read-only with a
    /// message to run fsck.
    fix_safe = 1,
    /// Immediately halt the entire machine, printing a backtrace on the
    /// system console
    panic = 2,
    /// Emergency read-only, immediately halting any changes to the
    /// filesystem on disk
    ro = 3,
}

#[bch_enum(open)]
#[repr(u32)]
pub enum DegradedAction {
    ask = 0,
    yes = 1,
    very = 2,
    no = 3,
}

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
#[bch_enum(open)]
#[prefix(BCH_WRITE_DEGRADED_)]
#[nr]
#[repr(u32)]
pub enum WriteDegradedAction {
    degraded = 0,
    yes = 1,
    no = 2,
}

#[bch_enum(open)]
#[prefix(BCH_STR_HASH_)]
#[nr]
#[repr(u32)]
pub enum StrHashKind {
    crc32c = 0,
    crc64 = 1,
    siphash_old = 2,
    siphash = 3,
}

#[bch_enum(open)]
#[prefix(BCH_STR_HASH_OPT_)]
#[nr]
#[repr(u32)]
pub enum StrHashOpt {
    crc32c = 0,
    crc64 = 1,
    siphash = 2,
}

#[bch_enum(open)]
#[prefix(BCH_CSUM_)]
#[nr]
#[repr(u32)]
pub enum CsumType {
    none = 0,
    crc32c_nonzero = 1,
    crc64_nonzero = 2,
    chacha20_poly1305_80 = 3,
    chacha20_poly1305_128 = 4,
    crc32c = 5,
    crc64 = 6,
    xxhash = 7,
}

#[bch_enum(open)]
#[prefix(BCH_CSUM_OPT_)]
#[nr]
#[repr(u32)]
pub enum CsumOpt {
    none = 0,
    crc32c = 1,
    crc64 = 2,
    xxhash = 3,
}

#[bch_enum(open)]
#[prefix(BCH_COMPRESSION_TYPE_)]
#[nr]
#[repr(u32)]
pub enum CompressionType {
    none = 0,
    lz4_old = 1,
    gzip = 2,
    lz4 = 3,
    zstd = 4,
    incompressible = 5,
}

#[bch_enum(open)]
#[prefix(BCH_COMPRESSION_OPT_)]
#[nr]
#[repr(u32)]
pub enum CompressionOpt {
    none = 0,
    lz4 = 1,
    gzip = 2,
    zstd = 3,
}

#[bch_enum(open)]
#[prefix(BCH_SCRUB_JOURNAL_)]
#[nr]
#[repr(u32)]
pub enum ScrubJournalOpt {
    unclean = 0,
    no = 1,
    always = 2,
}

/* statfs()'s f_type, and the superblock's s_magic: the kernel's
 * uapi/linux/magic.h has it too, as BCACHEFS_SUPER_MAGIC */
pub const BCACHEFS_STATFS_MAGIC: u32 = 0xca451a4e;

/* Journal */
#[allow(non_upper_case_globals)]
pub const JSET_KEYS_U64s: usize = size_of::<Head<c::jset_entry>>() / size_of::<u64>();

/* A journal entry is the type its type says: */
tagged_union! {
    #[storage(c::jset_entry)]
    #[tag(type_: u8)]
    #[tag_enum(JsetEntryType: u32, prefix = BCH_JSET_ENTRY_, nr)]
    #[size(header)]
    pub enum jset_entry {
        /// Btree key updates
        btree_keys(c::jset_entry_btree_keys) = 0,
        /// Btree root pointers, recorded every journal write
        btree_root(c::jset_entry_btree_root) = 1,
        /// Legacy, no longer used
        prio_ptrs(c::jset_entry_prio_ptrs) = 2,
        /// Blacklist a single journal sequence number
        blacklist(c::jset_entry_blacklist) = 3,
        /// Blacklist a range of journal sequence numbers
        blacklist_v2(c::jset_entry_blacklist_v2) = 4,
        /// Maximum key version for encryption nonce derivation
        usage(c::jset_entry_usage) = 5,
        /// Legacy: usage accounting moved to accounting btree
        data_usage(c::jset_entry_data_usage) = 6,
        /// IO clock: total reads and writes in sectors since filesystem
        /// creation
        clock(c::jset_entry_clock) = 7,
        /// Legacy: per-device usage moved to accounting btree
        dev_usage(c::jset_entry_dev_usage) = 8,
        /// Free-form log message for fsck actions and diagnostic events
        log(c::jset_entry_log) = 9,
        /// Old value being overwritten, for debugging and journal_rewind
        overwrite(c::jset_entry_overwrite) = 10,
        /// Write buffer keys, transformed to btree_keys before writing to disk
        write_buffer_keys(c::jset_entry_write_buffer_keys) = 11,
        /// Wall clock time at journal write
        datetime(c::jset_entry_datetime) = 12,
        /// Structured log entry containing a btree key
        log_bkey(c::jset_entry_log_bkey) = 13,
        /// Oldest journal seq safe for rewind (discards may have invalidated
        /// earlier seqs)
        rewind_limit(c::jset_entry_rewind_limit) = 14,
        /// Rewind in progress: keys from entries in this seq range use
        /// overwrite entries
        rewind(c::jset_entry_rewind) = 15,
    }
}

/* The entries whose payload is keys: */
macro_rules! jset_entry_keys {
    ($($name:ident),*) => { $(
        #[unsized_vstruct]
        #[repr(C)]
        #[derive(Default, CStruct, Plain, TypeInfo)]
        pub struct $name {
            pub entry: Head<c::jset_entry>,
            pub start: [c::bkey_i; 0],
            pub _data: [u64; 0],
        }
        vstruct!(unsized $name, entry.u64s);
        vstruct_container!($name, thin c::bkey_i);
    )* };
}
jset_entry_keys!(jset_entry_btree_keys, jset_entry_btree_root, jset_entry_overwrite,
                 jset_entry_write_buffer_keys, jset_entry_log_bkey);

/* Legacy, no longer written: */
#[unsized_vstruct]
#[repr(C)]
#[derive(Default, CStruct, Plain, TypeInfo)]
pub struct jset_entry_prio_ptrs {
    pub entry: Head<c::jset_entry>,
    pub _data: [u64; 0],
}
vstruct!(unsized jset_entry_prio_ptrs, entry.u64s);

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
#[whole]
#[repr(C)]
#[derive(Default, CStruct, Plain, TypeInfo)]
pub struct jset_entry_blacklist {
    pub entry: Head<c::jset_entry>,
    pub seq: Le64,
}
vstruct!(jset_entry_blacklist, repr.entry.u64s);

#[whole]
#[repr(C)]
#[derive(Default, CStruct, Plain, TypeInfo)]
pub struct jset_entry_blacklist_v2 {
    pub entry: Head<c::jset_entry>,
    pub start: Le64,
    pub end: Le64,
}
vstruct!(jset_entry_blacklist_v2, repr.entry.u64s);

#[bch_enum(open)]
#[prefix(BCH_FS_USAGE_)]
#[nr]
#[repr(u32)]
pub enum FsUsageType {
    reserved = 0,
    inodes = 1,
    key_version = 2,
}

#[whole]
#[repr(C, packed)]
#[derive(Default, CStruct, Plain, TypeInfo)]
pub struct jset_entry_usage {
    pub entry: Head<c::jset_entry>,
    pub v: Le64,
}
vstruct!(jset_entry_usage, repr.entry.u64s);

/* r's devs[] are in the tail: the entry's size is its u64s, not r's nr_devs */
#[unsized_vstruct(tail = [u8])]
#[repr(C, packed)]
#[derive(Default, CStruct, Plain, TypeInfo)]
pub struct jset_entry_data_usage {
    pub entry: Head<c::jset_entry>,
    pub v: Le64,
    pub r: Head<c::bch_replicas_entry_v1>,
}
vstruct!(unsized jset_entry_data_usage, entry.u64s);

#[whole]
#[repr(C, packed)]
#[derive(Default, CStruct, Plain, TypeInfo)]
pub struct jset_entry_clock {
    pub entry: Head<c::jset_entry>,
    pub rw: u8,
    pub pad: [u8; 7],
    pub time: Le64,
}
vstruct!(jset_entry_clock, repr.entry.u64s);

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
    pub entry: Head<c::jset_entry>,
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
pub struct jset_entry_log {
    pub entry: Head<c::jset_entry>,
    pub d: [u8; 0],
}
vstruct!(unsized jset_entry_log, entry.u64s);

#[whole]
#[repr(C, align(8))]
#[derive(Default, CStruct, Plain, TypeInfo)]
pub struct jset_entry_datetime {
    pub entry: Head<c::jset_entry>,
    pub seconds: Le64,
}
vstruct!(jset_entry_datetime, repr.entry.u64s);

#[whole]
#[repr(C, align(8))]
#[derive(Default, CStruct, Plain, TypeInfo)]
pub struct jset_entry_rewind_limit {
    pub entry: Head<c::jset_entry>,
    pub seq: Le64,
}
vstruct!(jset_entry_rewind_limit, repr.entry.u64s);

/*
 * Records a journal rewind in progress. Keys from journal entries
 * with seq in (to, from] use overwrite entries instead of btree_keys.
 */
#[whole]
#[repr(C, align(8))]
#[derive(Default, CStruct, Plain, TypeInfo)]
pub struct jset_entry_rewind {
    pub entry: Head<c::jset_entry>,
    pub from: Le64,
    pub to: Le64,
}
vstruct!(jset_entry_rewind, repr.entry.u64s);

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

    pub start: [Head<c::jset_entry>; 0],
    pub _data: [u64; 0],
}
vstruct!(unsized jset, u64s);
vstruct_container!(jset, jset_entry);

impl jset__head {
    /// By what holds it, which has room for its entries.
    pub fn set_u64s(&mut self, u64s: usize) {
        self.u64s.set(u64s as u32);
    }
}

/* A journal entry's keys, Views as its walk yields them: each one's header is whole. */
floored_by_self!(bkey_i);

c_bitmask! {
    LE32_BITMASK(struct Head<jset>, flags), strip JSET_ {
        JSET_CSUM_TYPE(0, 4),
        JSET_BIG_ENDIAN(4, 5),
        JSET_NO_FLUSH(5, 6),
        JSET_HAS_OVERWRITES(6, 7),
    }
}

pub const BCH_JOURNAL_BUCKETS_MIN: u32 = 8;

/* Btree: */

/*
 * Maximum number of btrees that we will _ever_ have under the current scheme,
 * where we refer to them with 64 bit bitfields - and we also need a bit for
 * the interior btree node type:
 */
pub const BTREE_ID_NR_MAX: u32 = 63;

pub const BTREE_MAX_DEPTH: u32 = 4;

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
vstruct_container!(bset, thin BkeyPHeader);

impl bset__head {
    /// By what holds it, which has room for its keys.
    pub fn set_u64s(&mut self, u64s: usize) {
        self.u64s.set(u64s as u16);
    }
}

/// Validated in place: its keys walked by offset, and it shortened - its
/// count, and the reference made over, so it still reaches all of it and
/// nothing past. The bytes past stay what holds it's.
impl bset<Unvalidated> {
    /// The key at byte @at of its keys, its u64s checked against what's left:
    /// None at their end.
    pub fn key_at(&self, at: usize) -> Option<Result<VstructRef<'_, BkeyPHeader>, VStructError>> {
        let keys = self.start_bytes();
        (at < keys.len()).then(|| VstructRef::new(&keys[at..]))
    }

    pub fn key_at_mut(&mut self, at: usize) -> Option<Result<VstructMut<'_, BkeyPHeader>, VStructError>> {
        let keys = self.start_bytes_mut();
        (at < keys.len()).then(|| VstructMut::new(&mut keys[at..]))
    }

    /// The first word of the key at byte @at, whatever its u64s say.
    pub fn key_header(&self, at: usize) -> Option<&BkeyPHeader> {
        cast_plain(self.start_bytes().get(at..)?)
    }

    /// Its keys from byte @at on gone.
    pub fn truncate(&mut self, at: usize) -> &mut Self {
        self.shrink(at)
    }

    /// The @bytes of keys at byte @at gone, the keys after moved down.
    pub fn remove(&mut self, at: usize, bytes: usize) -> &mut Self {
        let keys = self.start_bytes_mut();
        let len = keys.len();
        keys.copy_within(at + bytes.., at);
        self.shrink(len - bytes)
    }

    fn shrink(&mut self, keys_bytes: usize) -> &mut Self {
        let have = self.u64s() * 8;
        assert!(keys_bytes % 8 == 0 && keys_bytes <= have,
                "shrinking a bset of {have} bytes of keys to {keys_bytes}");

        let bytes = self.as_bytes_mut();
        cast_plain_mut::<Head<bset>>(bytes).expect("a bset's header").set_u64s(keys_bytes / 8);
        from_bytes_mut(&mut bytes[..offset_of!(Head<bset>, start) + keys_bytes]).expect("a bset, shorter")
    }
}

c_bitmask! {
    LE32_BITMASK(struct Head<bset>, flags), strip BSET_ {
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
pub struct btree_node {
    pub csum: c::bch_csum,
    pub magic: Le64,

    /* this flags field is encrypted, unlike bset->flags: */
    pub flags: Le64,

    /* Closed interval: */
    pub min_key: Bpos,
    pub max_key: Bpos,
    pub _ptr: c::bch_extent_ptr, /* not used anymore */
    pub format: c::bkey_format,

    /* C's vstruct macros find u64s and _data on the node itself: */
    pub keys: Head<c::bset>,
}
vstruct!(unsized btree_node, keys.u64s);
c_default!(btree_node__head);

c_bitmask! {
    LE64_BITMASK(struct Head<btree_node>, flags), strip BTREE_NODE_ {
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
pub struct btree_node_entry {
    pub csum: c::bch_csum,

    /* C's vstruct macros find u64s and _data on the entry itself: */
    pub keys: Head<c::bset>,
}
vstruct!(unsized btree_node_entry, keys.u64s);
c_default!(btree_node_entry__head);
