// SPDX-License-Identifier: GPL-2.0

//! The data types of vfs/types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::CStruct;
use typeinfo_macros::TypeInfo;


/*
 * An inode number that something in memory is holding: membership in
 * @inodes_by_inum_table below.
 *
 * bch2_inode_or_descendents_is_open() consults that table and nothing else, so
 * that fsck won't delete an unlinked inode out from under a live reference.
 * Membership - not the existence of a bch_inode_info - is therefore what
 * matters, and this is a standalone type so an entry can be held without one:
 * an unlinked on-disk inode becomes visible to a scanning fsck pass at
 * bch2_trans_commit(), which for O_TMPFILE is before __bch2_create() has a VFS
 * inode to hash.
 *
 * @inum is kept whole rather than split into its fields because
 * @inodes_table keys on the entire subvol_inum.
 */
#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_inum_hash_entry {
    pub hash: c::rhlist_head,
    pub inum: c::subvol_inum,
}
c_default!(bch_inum_hash_entry);

#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_fs_vfs {
    pub inodes: c::fast_list,
    pub inodes_table: c::rhashtable,
    /*
     * Keyed on the inode number alone, not (subvol, inum): a query asks
     * whether any snapshot version of an inode number is held, so every
     * version has to land in one bucket. Hence an rhltable - the key is
     * deliberately non-unique.
     */
    pub inodes_by_inum_table: c::rhltable,

    pub writepage_bioset: c::bio_set,
    pub dio_write_bioset: c::bio_set,
    pub dio_read_bioset: c::bio_set,
    pub nocow_flush_bioset: c::bio_set,
    pub writepage_buf_pool: c::mempool_t,
    pub writeback_wq: *mut c::workqueue_struct,
}
c_default!(bch_fs_vfs);
