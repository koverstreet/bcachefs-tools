// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

pub mod attr;
pub mod buffered;
pub mod buffered_write;
pub mod dentry;
pub mod dir;
pub mod direct;
pub mod direct_write;
pub mod export;
pub mod fdm;
pub mod fiemap;
pub mod file;
pub mod folio;
pub mod folio_reservation;
pub mod fs_type;
pub mod inode;
pub mod inode_cache;
pub mod inode_ops;
pub mod io;
pub mod namei;
pub mod new_inode;
pub mod pagecache;
pub mod path;
pub mod status_fd;
pub mod super_block;
pub mod ioctl;
pub mod writeback;

use crate::data_structures::fast_list::fast_list;
use crate::errcode::Errcode;
use crate::vfs::pagecache::BCH_WRITEPAGE_BUF_BYTES;
use crate::os::workqueue::{Workqueue, WqFlags};
use crate::os::bio::BioSet;
use crate::util::{late::Late, mempool::Mempool};
use cstruct_macros::{MaybeZeroable, Init, Late, TypeInfo};
use pin_init::pin_data;

#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, TypeInfo, Init, Late)]
pub struct bch_fs_vfs {
    #[pin]
    #[init(crate::util::alloc::init_or(fast_list::new(), Errcode::ENOMEM_fs_other_alloc))]
    pub inodes: fast_list,
    /* every VFS inode of ours, and which subvolumes hold an inode number */
    #[pin]
    #[init(crate::vfs::inode_cache::InodeCache::default())]
    pub(crate) inode_cache: crate::vfs::inode_cache::InodeCache,

    #[late(BioSet::new(1), ENOMEM_nocow_flush_bioset_init)]
    pub nocow_flush_bioset: Late<BioSet<crate::data::dev_bio::DevBio>>,
    #[late(Mempool::new(4, BCH_WRITEPAGE_BUF_BYTES), ENOMEM_writepage_buf_pool_init)]
    pub writepage_buf_pool: Late<Mempool>,
    #[late(Workqueue::new(c"bcachefs_vfs_writeback",
                          WqFlags::MEM_RECLAIM | WqFlags::FREEZABLE | WqFlags::UNBOUND, 1),
           ENOMEM_fs_other_alloc)]
    pub writeback_wq: Late<Workqueue>,
}
