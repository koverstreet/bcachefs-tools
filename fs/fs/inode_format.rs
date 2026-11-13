// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{Le16, Le32, Le64};
use cstruct_macros::{c_bitmask, bch_enum, c_xmacro, whole, CStruct, Plain, TypeInfo, SizedVal};

pub const BLOCKDEV_INODE_MAX: u32 = 4096;
pub const BCACHEFS_ROOT_INO: u32 = 4096;

#[whole(tail = fields: [u8], bkey)]
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct Inode {
    pub v: c::bch_val,

    pub bi_hash_seed: Le64,
    pub bi_flags: Le32,
    pub bi_mode: Le16,
    pub fields: [u8; 0],
}

#[whole(tail = fields: [u8], bkey)]
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct InodeV2 {
    pub v: c::bch_val,

    pub bi_journal_seq: Le64,
    pub bi_hash_seed: Le64,
    pub bi_flags: Le64,
    pub bi_mode: Le16,
    pub fields: [u8; 0],
}

#[whole(tail = fields: [u8], bkey)]
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct InodeV3 {
    pub v: c::bch_val,

    pub bi_journal_seq: Le64,
    pub bi_hash_seed: Le64,
    pub bi_flags: Le64,
    pub bi_sectors: Le64,
    pub bi_size: Le64,
    pub bi_version: Le64,
    pub fields: [u8; 0],
}

#[allow(non_upper_case_globals)]
pub const INODEv3_FIELDS_START_INITIAL: u32 = 6;

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo, SizedVal)]
pub struct InodeGeneration {
    pub v: c::bch_val,

    pub bi_generation: Le32,
    pub pad: Le32,
}

c_xmacro! {
    /*
     * bi_subvol and bi_parent_subvol are only set for subvolume roots:
     */
    BCH_INODE_FIELDS_v2 {
        (bi_atime, 96),
        (bi_ctime, 96),
        (bi_mtime, 96),
        (bi_otime, 96),
        (bi_size, 64),
        (bi_sectors, 64),
        (bi_uid, 32),
        (bi_gid, 32),
        (bi_nlink, 32),
        (bi_generation, 32),
        (bi_dev, 32),
        (bi_data_checksum, 8),
        (bi_compression, 8),
        (bi_project, 32),
        (bi_background_compression, 8),
        (bi_data_replicas, 8),
        (bi_promote_target, 16),
        (bi_foreground_target, 16),
        (bi_background_target, 16),
        (bi_erasure_code, 16),
        (bi_fields_set, 16),
        (bi_dir, 64),
        (bi_dir_offset, 64),
        (bi_subvol, 32),
        (bi_parent_subvol, 32),
    }
}

c_xmacro! {
    BCH_INODE_FIELDS_v3 {
        (bi_atime, 96),
        (bi_ctime, 96),
        (bi_mtime, 96),
        (bi_otime, 96),
        (bi_uid, 32),
        (bi_gid, 32),
        (bi_nlink, 32),
        (bi_generation, 32),
        (bi_dev, 32),
        (bi_data_checksum, 8),
        (bi_compression, 8),
        (bi_project, 32),
        (bi_background_compression, 8),
        (bi_data_replicas, 8),
        (bi_promote_target, 16),
        (bi_foreground_target, 16),
        (bi_background_target, 16),
        (bi_erasure_code, 16),
        (bi_fields_set, 16),
        (bi_dir, 64),
        (bi_dir_offset, 64),
        (bi_subvol, 32),
        (bi_parent_subvol, 32),
        (bi_nocow, 8),
        (bi_depth, 32),
        (bi_inodes_32bit, 8),
        (bi_casefold, 8),
        (bi_unused_ec_max_data_blocks, 8),
    }
}

#[bch_enum(open)]
#[prefix(Inode_opt_)]
#[nr]
#[arm_columns(bits: value(u32))]
/* subset of BCH_INODE_FIELDS */
#[repr(u32)]
pub enum InodeOptId {
    #[bits(8)]  data_checksum,
    #[bits(8)]  compression,
    #[bits(32)] project,
    #[bits(8)]  background_compression,
    #[bits(8)]  data_replicas,
    #[bits(16)] promote_target,
    #[bits(16)] foreground_target,
    #[bits(16)] background_target,
    #[bits(16)] erasure_code,
    #[bits(8)]  nocow,
    #[bits(8)]  inodes_32bit,
    #[bits(8)]  casefold,
}

/* bits 20+ reserved for packed fields below: */
#[bch_enum(open)]
#[prefix(__BCH_INODE_)]
#[mask(InodeFlags: u32, prefix = BCH_INODE_)]
/*
 * BCH_INODE_unlinked means we definitively know that there isn't, and
 * shouldn't be, a dirent pointing to this inode: it is (or is about to
 * be) deleted. check_unreachable_inodes() keys off it to distinguish
 * "unreachable by design" from damage needing reattach, and setting it
 * enrolls the inode in the deleted_inodes btree (bch2_trigger_inode())
 * so a crash can't leak the inode.
 *
 * Ordinary inodes carrying it are deleted by the inode reaper - VFS
 * eviction -> bch2_inode_rm(), or bch2_delete_dead_inodes() after a
 * crash. Subvolume root inodes carry it too once their subvolume has
 * been unlinked, but their deletion belongs to the subvolume deletion
 * path (subvolume state unlinked -> VFS eviction -> snapshot sweep) and
 * the inode reaper must leave them alone - see
 * bch2_inode_is_subvolume_root().
 *
 * BCH_INODE_has_case_insensitive is set if any descendent is case insensitive -
 * for overlayfs
 *
 * BCH_INODE_has_inode_opts is set iff some field in BCH_INODE_OPTS() is
 * nonzero on this inode (per-inode option overrides exist). Lets the
 * extent-update path skip the inode unpack and use fs defaults when the
 * bit is clear. Maintained by inode::pack(): recomputed from the
 * BCH_INODE_OPTS fields every pack.
 *
 * BCH_INODE_has_access_acl / has_default_acl are set iff a POSIX ACL
 * xattr of that type (KEY_TYPE_XATTR_INDEX_POSIX_ACL_ACCESS / _DEFAULT)
 * exists for this inode, so the common no-ACL path in bch2_get_acl() can
 * short-circuit without an xattr lookup. Maintained at inode create and
 * in bch2_set_acl_trans().
 *
 * All three ride the per_dev_fragmentation_lru version: the upgrade
 * schedules check_inodes + check_xattrs, which set the flags correctly
 * on existing inodes (silently - the upgrade entry lists the flag
 * errors, feeding errors_silent), and fsck verifies both directions
 * from then on. Version upgrades always run, so consumers can trust
 * the bits unconditionally.
 */
#[repr(u32)]
pub enum InodeFlagBit {
    sync = 0,
    immutable = 1,
    append = 2,
    nodump = 3,
    noatime = 4,
    i_size_dirty = 5,
    i_sectors_dirty = 6,
    unlinked = 7,
    backptr_untrusted = 8,
    has_child_snapshot = 9,
    has_case_insensitive = 10,
    __BCH_INODE_31bit_dirent_offset = 11,
    has_inode_opts = 12,
    has_access_acl = 13,
    has_default_acl = 14,
}

c_bitmask! {
    LE32_BITMASK(struct Inode, bi_flags) {
        INODEv1_STR_HASH(20, 24),
        INODEv1_NR_FIELDS(24, 31),
        INODEv1_NEW_VARINT(31, 32),
    }
}

c_bitmask! {
    LE64_BITMASK(struct InodeV2, bi_flags) {
        INODEv2_STR_HASH(20, 24),
        INODEv2_NR_FIELDS(24, 31),
    }
}

c_bitmask! {
    LE64_BITMASK(struct InodeV3, bi_flags) {
        INODEv3_STR_HASH(20, 24),
        INODEv3_NR_FIELDS(24, 31),
        INODEv3_FIELDS_START(31, 36),
        INODEv3_MODE(36, 52),
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo, SizedVal)]
pub struct InodeAllocCursor {
    pub v: c::bch_val,
    pub bits: u8,
    pub pad: u8,
    /* C's padding, for __le32's alignment */
    pub __gen_align: [u8; 2],
    pub generation: Le32,
    pub idx: Le64,
}
