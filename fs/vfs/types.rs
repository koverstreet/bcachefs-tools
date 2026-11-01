// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
#[cfg(not(NO_BCACHEFS_FS))]
use crate::util::{bio::BioSet, late::Late, mempool::Mempool, workqueue::{Workqueue, WqFlags}};
use cstruct_macros::{CStruct, Init, Late, TypeInfo};
use pin_init::pin_data;

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

#[pin_data]
#[repr(C)]
#[derive(CStruct, TypeInfo, Init, Late)]
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

    #[cfg(not(NO_BCACHEFS_FS))]
    #[late(BioSet::new(4, true), ENOMEM_writepage_bioset_init)]
    #[c("struct bio_set writepage_bioset")]
    pub writepage_bioset: Late<BioSet<c::bch_writepage_io>>,
    #[cfg(not(NO_BCACHEFS_FS))]
    #[late(BioSet::new(4, true), ENOMEM_dio_write_bioset_init)]
    #[c("struct bio_set dio_write_bioset")]
    pub dio_write_bioset: Late<BioSet<c::dio_write>>,
    #[cfg(not(NO_BCACHEFS_FS))]
    #[pin]
    #[init(BioSet::new(4, true), ENOMEM_dio_read_bioset_init)]
    #[c("struct bio_set dio_read_bioset")]
    pub dio_read_bioset: BioSet<c::dio_read>,
    #[cfg(not(NO_BCACHEFS_FS))]
    #[late(BioSet::new(1, false), ENOMEM_nocow_flush_bioset_init)]
    #[c("struct bio_set nocow_flush_bioset")]
    pub nocow_flush_bioset: Late<BioSet<c::nocow_flush>>,
    #[cfg(not(NO_BCACHEFS_FS))]
    #[late(Mempool::kvmalloc(4, c::BCH_WRITEPAGE_BUF_BYTES), ENOMEM_writepage_buf_pool_init)]
    #[c("mempool_t writepage_buf_pool")]
    pub writepage_buf_pool: Late<Mempool>,
    #[cfg(not(NO_BCACHEFS_FS))]
    #[late(Workqueue::new(c"bcachefs_vfs_writeback",
                          WqFlags::MEM_RECLAIM | WqFlags::FREEZABLE | WqFlags::UNBOUND, 1),
           ENOMEM_fs_other_alloc)]
    #[c("struct workqueue_struct *writeback_wq")]
    pub writeback_wq: Late<Workqueue>,
}
c_default!(bch_fs_vfs);
