// SPDX-License-Identifier: GPL-2.0

//! A new VFS inode: allocated, made from the btree's inode, and not yet in
//! the inode cache - its creator's alone, so what's set of it is set through
//! &mut, until InodeCache::insert() takes it. Dropped instead, it's given up
//! as a bad inode, which evict doesn't delete.
//!
//! Inodes are INODE_CACHE's, a slab cache made at module init; the VFS frees
//! them, a grace period after evict, through free().

use crate::btree::iter::{BtreeTrans, TransAttempt};
use crate::c::{self, two_state_lock_init, InodeQuota, InodeReserved, InodeReservedRange};
use crate::errcode::{ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::locking::MemallocFlags;
use crate::new_lock;
use crate::util::alloc::{flags, AllocError, Flags};
use crate::os::module::{Quiesced, ThisModule};
use crate::os::slab::KmemCache;
use crate::os::workqueue::{run_raw, RawWork};
use core::mem::{offset_of, ManuallyDrop};
use core::ops::{Deref, DerefMut};
use core::ptr::NonNull;
use kernel::sync::aref::ARef;
use pin_init::{pin_init, pin_init_from_closure, Zeroable};

use super::inode::{InodeInfo, InodeRef};
use super::pagecache::INODE_AOPS;
use crate::os::vfs_inode::VfsInode;

pub(crate) static INODE_CACHE: KmemCache<InodeInfo> = KmemCache::new();

pub struct NewInode(NonNull<InodeInfo>);

// SAFETY: an inode, and a reference to it, which any thread may put
unsafe impl Send for NewInode {}

impl NewInode {
    /// An inode with its locks, timer and place on the filesystem's list, and
    /// what inode_init_always() gives it - or none, if there's no memory as
    /// @gfp allows.
    pub fn new(fs: &Fs, gfp: Flags) -> Option<Self> {
        let sb = fs.vfs_super().expect("an inode for a filesystem with no VFS superblock");
        let _gfp = MemallocFlags::for_gfp(gfp);
        let slot = fs.vfs.inodes.get_slot().ok()?;

        let init = pin_init!(InodeInfo {
            v <- VfsInode::new(sb, gfp),
            ei_reserved <- new_lock!(InodeReserved { ei_reserved_replicas: 0, ei_reserved_state: 0 },
                                     InodeReservedRange { ei_reserved_start: Zeroable::zeroed(),
                                                          ei_reserved_end: Zeroable::zeroed() },
                                     "&inode->ei_reserved_lock"),
            ei_update_lock <- new_lock!((), "&inode->ei_update_lock"),
            // SAFETY: initialized in place, as two_state_lock_init() does
            ei_pagecache_lock <- unsafe {
                pin_init_from_closure(|l: *mut c::two_state_lock_t| {
                    two_state_lock_init(l);
                    Ok::<_, core::convert::Infallible>(())
                })
            },
            ei_quota <- new_lock!(InodeQuota { ei_qid: Default::default(), ei_quota_reserved: 0 },
                                  "&inode->ei_quota_lock"),
            ei_writeback_timer <- crate::new_delayed_work!(run_raw::<InodeInfo>, "&inode->ei_writeback_timer"),
            ei_inum: Zeroable::zeroed(),
            ei_inodes_idx: 0,
            ei_flags: Zeroable::zeroed(),
            ei_last_dirtied: Zeroable::zeroed(),
            ei_devs_need_flush: Zeroable::zeroed(),
            ei_inode: Zeroable::zeroed(),
        }? BchError);

        let mut inode = NewInode(INODE_CACHE.alloc_inode(sb, gfp, init)?);
        inode.ei_inodes_idx = slot.into_idx();
        // From the start, as FsMapping has it: inode_init_always() set the
        // VFS's empty ones
        inode.v.set_aops(&INODE_AOPS);
        Some(inode)
    }

    /// As new(), in @t: without blocking under its locks - else with them
    /// dropped, and a restart if they can't be had back.
    pub fn new_trans(t: &TransAttempt<'_, '_>) -> Result<Self, BchError> {
        let fs = t.fs();
        if let Some(inode) = Self::new(fs, flags::NOWAIT) {
            return Ok(inode);
        }
        t.drop_locks_do(|| Self::new(fs, flags::NOIO).ok_or_else(|| AllocError.into()))
    }

    /// Made from @bi - inode @inum, in @subvol.
    pub fn init(&mut self, trans: &BtreeTrans<'_>, inum: c::subvol_inum, bi: &c::bch_inode_unpacked,
                subvol: &c::Subvolume) {
        let block_bytes = self.fs().block_bytes();

        self.v.set_ino(inum.inum);
        self.ei_inum = inum;
        self.update_after_write(trans, bi, !0);

        self.v.set_blocks(bi.bi_sectors);
        self.v.set_rdev(bi.bi_dev as _);
        self.v.set_generation(bi.bi_generation);
        self.v.set_size(bi.bi_size);

        *self.ei_quota.get_mut() = InodeQuota { ei_qid: crate::quota::qid(bi), ei_quota_reserved: 0 };
        if subvol.snap() {
            self.ei_flags.set(c::InodeInfoFlag::EI_INODE_SNAPSHOT, true);
        }

        match self.v.mode() as u32 & c::S_IFMT {
            c::S_IFREG => {
                self.v.set_ops(&super::inode_ops::FILE_INODE_OPS);
                self.v.set_fop(&super::file::FILE_OPS);
            }
            c::S_IFDIR => {
                self.v.set_ops(&super::inode_ops::DIR_INODE_OPS);
                self.v.set_fop(&super::dir::DIR_FILE_OPS);
            }
            c::S_IFLNK => {
                self.v.nohighmem();
                self.v.set_ops(&super::inode_ops::SYMLINK_INODE_OPS);
            }
            _ => {
                self.v.init_special();
                self.v.set_ops(&super::inode_ops::SPECIAL_INODE_OPS);
            }
        }
        self.v.set_folio_min_size(block_bytes as usize);
    }

    /// For InodeCache::insert(): the reference, the cache's to give back.
    pub(super) fn into_ref(self) -> InodeRef {
        // SAFETY: the reference new() took
        unsafe { ARef::from_raw(ManuallyDrop::new(self).0) }
    }
}

impl Deref for NewInode {
    type Target = InodeInfo;

    fn deref(&self) -> &InodeInfo {
        // SAFETY: live while we hold it
        unsafe { self.0.as_ref() }
    }
}

impl DerefMut for NewInode {
    fn deref_mut(&mut self) -> &mut InodeInfo {
        // SAFETY: never in the cache, nor on the superblock's list: ours alone
        unsafe { self.0.as_mut() }
    }
}

impl Drop for NewInode {
    /// Given up: a bad inode, which evict doesn't delete, and our reference
    /// put.
    fn drop(&mut self) {
        self.v.make_bad();
        // SAFETY: the reference new() took
        drop(unsafe { InodeRef::from_raw(self.0) });
    }
}

/// The writeback timer's: the inode written back, if it's live. Evict cancels
/// it before the inode's freed.
impl RawWork for InodeInfo {
    const WORK: usize = offset_of!(InodeInfo, ei_writeback_timer) + offset_of!(c::delayed_work, work);

    unsafe fn run(this: NonNull<Self>) {
        if let Some(r) = unsafe { this.as_ref() }.vfs().igrab() {
            let _ = r.write_now(false);
        }
    }
}

/// The VFS's, a grace period after evict: free_inode().
pub(super) fn free(inode: NonNull<InodeInfo>) {
    // SAFETY: INODE_CACHE's, and the VFS is done with it
    unsafe { INODE_CACHE.free(inode) }
}

/// At module init: the inode cache, then the filesystem type. Failed, module
/// exit undoes what was done.
pub fn vfs_init(m: &ThisModule) -> Result<(), BchError> {
    INODE_CACHE.create(c"bch_inode_info", c::SLAB_RECLAIM_ACCOUNT | c::SLAB_ACCOUNT)?;
    super::direct::pools_init(m)?;
    super::buffered::pools_init(m)?;
    ret_to_result_void(super::fs_type::bch2_fs_type_register())
}

/// Past the grace period free() waits: the last inode's back in the cache.
pub fn vfs_exit(q: &Quiesced<'_>) {
    super::fs_type::bch2_fs_type_unregister();
    INODE_CACHE.destroy(q);
}
