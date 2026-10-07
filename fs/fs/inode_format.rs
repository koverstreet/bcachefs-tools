// SPDX-License-Identifier: GPL-2.0

//! The data types of fs/inode_format.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use zerocopy::byteorder::little_endian as le;
use cstruct_macros::{c_bitmask, c_const, c_enum, c_verbatim, c_xmacro, CStruct};
use typeinfo_macros::TypeInfo;

c_const! {
    #[c_int]
    pub const BLOCKDEV_INODE_MAX: u32 = 4096;
}

c_const! {
    #[c_int]
    pub const BCACHEFS_ROOT_INO: u32 = 4096;
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_inode {
    pub v: c::bch_val,

    pub bi_hash_seed: le::U64,
    pub bi_flags: le::U32,
    pub bi_mode: le::U16,
    pub fields: [u8; 0],
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_inode_v2 {
    pub v: c::bch_val,

    pub bi_journal_seq: le::U64,
    pub bi_hash_seed: le::U64,
    pub bi_flags: le::U64,
    pub bi_mode: le::U16,
    pub fields: [u8; 0],
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_inode_v3 {
    pub v: c::bch_val,

    pub bi_journal_seq: le::U64,
    pub bi_hash_seed: le::U64,
    pub bi_flags: le::U64,
    pub bi_sectors: le::U64,
    pub bi_size: le::U64,
    pub bi_version: le::U64,
    pub fields: [u8; 0],
}

c_const! {
    #[c_int]
    pub const INODEv3_FIELDS_START_INITIAL: u32 = 6;
}

c_verbatim!(r#"
#define INODEv3_FIELDS_START_CUR	(offsetof(struct bch_inode_v3, fields) / sizeof(__u64))
"#);

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_inode_generation {
    pub v: c::bch_val,

    pub bi_generation: le::U32,
    pub pad: le::U32,
}

c_xmacro! {
    /*
     * bi_subvol and bi_parent_subvol are only set for subvolume roots:
     */
    BCH_INODE_FIELDS_v2(x) {
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
    BCH_INODE_FIELDS_v3(x) {
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

c_xmacro! {
    /* subset of BCH_INODE_FIELDS */
    BCH_INODE_OPTS(x) {
        (data_checksum, 8),
        (compression, 8),
        (project, 32),
        (background_compression, 8),
        (data_replicas, 8),
        (promote_target, 16),
        (foreground_target, 16),
        (background_target, 16),
        (erasure_code, 16),
        (nocow, 8),
        (inodes_32bit, 8),
        (casefold, 8),
    }
}

macro_rules! __inode_opt_id_0 {
    ([$($acc:tt)*] $(($name:tt$(, $($__rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum inode_opt_id: u32 {
                $($acc)*
                $([<Inode_opt_ $name>],)*
                Inode_opt_nr,
            }
        }
    } };
}
BCH_INODE_OPTS!(__inode_opt_id_0 []);

c_xmacro! {
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
     * bit is clear. Maintained by bch2_inode_pack(): recomputed from the
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
    BCH_INODE_FLAGS(x) {
        (sync, 0),
        (immutable, 1),
        (append, 2),
        (nodump, 3),
        (noatime, 4),
        (i_size_dirty, 5),
        (i_sectors_dirty, 6),
        (unlinked, 7),
        (backptr_untrusted, 8),
        (has_child_snapshot, 9),
        (has_case_insensitive, 10),
        (31bit_dirent_offset, 11),
        (has_inode_opts, 12),
        (has_access_acl, 13),
        (has_default_acl, 14),
    }
}

/* bits 20+ reserved for packed fields below: */
macro_rules! __bch_inode_flags_0 {
    ([$($acc:tt)*] $(($t:tt, $n:expr)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[flags]
            pub enum bch_inode_flags: u32 {
                $($acc)*
                $([<BCH_INODE_ $t>] = 1 << (($n) as u32),)*
            }
        }
    } };
}
BCH_INODE_FLAGS!(__bch_inode_flags_0 []);

macro_rules! ____bch_inode_flags_0 {
    ([$($acc:tt)*] $(($t:tt, $n:expr)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum __bch_inode_flags: u32 {
                $($acc)*
                $([<__BCH_INODE_ $t>] = (($n) as u32),)*
            }
        }
    } };
}
BCH_INODE_FLAGS!(____bch_inode_flags_0 []);

c_bitmask! {
    LE32_BITMASK(struct bch_inode, bi_flags) {
        INODEv1_STR_HASH(20, 24),
        INODEv1_NR_FIELDS(24, 31),
        INODEv1_NEW_VARINT(31, 32),
    }
}

c_bitmask! {
    LE64_BITMASK(struct bch_inode_v2, bi_flags) {
        INODEv2_STR_HASH(20, 24),
        INODEv2_NR_FIELDS(24, 31),
    }
}

c_bitmask! {
    LE64_BITMASK(struct bch_inode_v3, bi_flags) {
        INODEv3_STR_HASH(20, 24),
        INODEv3_NR_FIELDS(24, 31),
        INODEv3_FIELDS_START(31, 36),
        INODEv3_MODE(36, 52),
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_inode_alloc_cursor {
    pub v: c::bch_val,
    pub bits: u8,
    pub pad: u8,
    #[c_anon("")] pub __generation_align: [u32; 0],
    pub generation: le::U32,
    pub idx: le::U64,
}
