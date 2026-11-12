// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
#[cfg(not(NO_BCACHEFS_FS))]
use crate::types::{RustOnly, rust_only};
#[cfg(not(NO_BCACHEFS_FS))]
use crate::util::{alloc::KVmalloc, bio::BioSet, late::Late, mempool::Mempool, workqueue::{Workqueue, WqFlags}};
use cstruct_macros::{CStruct, Init, Late, TypeInfo};
use pin_init::pin_data;

#[pin_data]
#[repr(C)]
#[derive(CStruct, TypeInfo, Init, Late)]
pub struct bch_fs_vfs {
    pub inodes: c::fast_list,
    /* every VFS inode of ours, and which subvolumes hold an inode number */
    #[cfg(not(NO_BCACHEFS_FS))]
    #[pin]
    #[init(RustOnly::new(crate::vfs::inode_cache::InodeCache::default()))]
    pub(crate) inode_cache: rust_only!(crate::vfs::inode_cache::InodeCache),

    #[cfg(not(NO_BCACHEFS_FS))]
    #[late(BioSet::new(4, true), ENOMEM_writepage_bioset_init)]
    #[c("struct bio_set writepage_bioset")]
    pub writepage_bioset: Late<BioSet<c::bch_writepage_io>>,
    #[cfg(not(NO_BCACHEFS_FS))]
    #[late(BioSet::new(4, true), ENOMEM_dio_write_bioset_init)]
    #[c("struct bio_set dio_write_bioset")]
    pub dio_write_bioset: Late<BioSet<c::dio_write>>,
    #[cfg(not(NO_BCACHEFS_FS))]
    #[late(BioSet::new(1, false), ENOMEM_nocow_flush_bioset_init)]
    #[c("struct bio_set nocow_flush_bioset")]
    pub nocow_flush_bioset: Late<BioSet<crate::vfs::io::NocowFlush>>,
    #[cfg(not(NO_BCACHEFS_FS))]
    #[late(Mempool::kvmalloc(4, c::BCH_WRITEPAGE_BUF_BYTES), ENOMEM_writepage_buf_pool_init)]
    #[c("mempool_t writepage_buf_pool")]
    pub writepage_buf_pool: Late<Mempool<KVmalloc>>,
    #[cfg(not(NO_BCACHEFS_FS))]
    #[late(Workqueue::new(c"bcachefs_vfs_writeback",
                          WqFlags::MEM_RECLAIM | WqFlags::FREEZABLE | WqFlags::UNBOUND, 1),
           ENOMEM_fs_other_alloc)]
    #[c("struct workqueue_struct *writeback_wq")]
    pub writeback_wq: Late<Workqueue>,
}
