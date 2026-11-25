// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
#[cfg(kernel)]
use crate::os::workqueue::{Workqueue, WqFlags};
#[cfg(kernel)]
use crate::os::bio::BioSet;
#[cfg(kernel)]
use crate::util::{late::Late, mempool::Mempool};
use cstruct_macros::{MaybeZeroable, Init, Late, TypeInfo};
use pin_init::pin_data;

#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, TypeInfo, Init, Late)]
pub struct bch_fs_vfs {
    #[pin]
    #[init(crate::util::alloc::init_or(c::fast_list::new(), c::Errcode::ENOMEM_fs_other_alloc))]
    pub inodes: c::fast_list,
    /* every VFS inode of ours, and which subvolumes hold an inode number */
    #[cfg(kernel)]
    #[pin]
    #[init(crate::vfs::inode_cache::InodeCache::default())]
    pub(crate) inode_cache: crate::vfs::inode_cache::InodeCache,

    #[cfg(kernel)]
    #[late(BioSet::new(1), ENOMEM_nocow_flush_bioset_init)]
    pub nocow_flush_bioset: Late<BioSet<crate::data::dev_bio::DevBio>>,
    #[cfg(kernel)]
    #[late(Mempool::new(4, c::BCH_WRITEPAGE_BUF_BYTES), ENOMEM_writepage_buf_pool_init)]
    pub writepage_buf_pool: Late<Mempool>,
    #[cfg(kernel)]
    #[late(Workqueue::new(c"bcachefs_vfs_writeback",
                          WqFlags::MEM_RECLAIM | WqFlags::FREEZABLE | WqFlags::UNBOUND, 1),
           ENOMEM_fs_other_alloc)]
    pub writeback_wq: Late<Workqueue>,
}
