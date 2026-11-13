// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
#[cfg(not(NO_BCACHEFS_FS))]
#[cfg(not(NO_BCACHEFS_FS))]
use crate::util::{alloc::KVmalloc, bio::BioSet, late::Late, mempool::Mempool, workqueue::{Workqueue, WqFlags}};
use cstruct_macros::{MaybeZeroable, Init, Late, TypeInfo};
use pin_init::pin_data;

#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, TypeInfo, Init, Late)]
pub struct bch_fs_vfs {
    pub inodes: c::fast_list,
    /* every VFS inode of ours, and which subvolumes hold an inode number */
    #[cfg(not(NO_BCACHEFS_FS))]
    #[pin]
    #[init(crate::vfs::inode_cache::InodeCache::default())]
    pub(crate) inode_cache: crate::vfs::inode_cache::InodeCache,

    #[cfg(not(NO_BCACHEFS_FS))]
    #[late(BioSet::new(4, true), ENOMEM_writepage_bioset_init)]
    pub writepage_bioset: Late<BioSet<c::bch_writepage_io>>,
    #[cfg(not(NO_BCACHEFS_FS))]
    #[late(BioSet::new(4, true), ENOMEM_dio_write_bioset_init)]
    pub dio_write_bioset: Late<BioSet<c::dio_write>>,
    #[cfg(not(NO_BCACHEFS_FS))]
    #[late(BioSet::new(1, false), ENOMEM_nocow_flush_bioset_init)]
    pub nocow_flush_bioset: Late<BioSet<crate::vfs::io::NocowFlush>>,
    #[cfg(not(NO_BCACHEFS_FS))]
    #[late(Mempool::kvmalloc(4, c::BCH_WRITEPAGE_BUF_BYTES), ENOMEM_writepage_buf_pool_init)]
    pub writepage_buf_pool: Late<Mempool<KVmalloc>>,
    #[cfg(not(NO_BCACHEFS_FS))]
    #[late(Workqueue::new(c"bcachefs_vfs_writeback",
                          WqFlags::MEM_RECLAIM | WqFlags::FREEZABLE | WqFlags::UNBOUND, 1),
           ENOMEM_fs_other_alloc)]
    pub writeback_wq: Late<Workqueue>,
}
