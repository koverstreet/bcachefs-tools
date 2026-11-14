// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::locking::Locked;
use crate::published;
use crate::types::c_default;
use crate::data_structures::bitmap::AtomicBitflags;
use crate::util::projections::Volatile;
use crate::util::workqueue::DelayedWork;
use crate::vfs::vfs_inode::VfsInode;
use core::ffi::{c_uint, c_ulong};
#[cfg(not(NO_BCACHEFS_FS))]
use core::ffi::{c_int, c_void};
use cstruct_macros::{bch_enum, MaybeZeroable, TypeInfo};
use pin_init::pin_data;
#[cfg(not(NO_BCACHEFS_FS))]
use crate::btree::iter::BtreeTrans;

/*
 * The VFS's inode, and what we keep beside it. Shared - the kernel and C use it
 * while Rust has a &InodeInfo - so what Rust touches is typed for that: @v,
 * the struct inode the kernel writes, a VfsInode; ei_inode a
 * VolatileInodeUnpacked; ei_update_lock and ei_quota_lock Locked, the second
 * over what it protects.
 */
#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, TypeInfo)]
pub struct InodeInfo {
    #[pin]
    pub v: VfsInode,
    /* the inode cache's key: set before it's in, unchanged while it is */
    pub ei_inum: c::subvol_inum,

    #[pin]
    pub ei_reserved: Locked<c::spinlock_t, InodeReserved, InodeReservedRange>,

    pub ei_inodes_idx: c_uint,
    pub ei_flags: AtomicBitflags<c::InodeInfoFlag>,

    #[pin]
    pub ei_update_lock: Locked<c::mutex>,
    pub ei_last_dirtied: Volatile<c_ulong>,
    #[pin]
    pub ei_pagecache_lock: c::two_state_lock_t,

    /*
     * The quota the inode's usage is charged to: i_blocks + ei_quota_reserved
     * is charged to ei_qid. i_blocks, the VFS inode's, changes under the lock
     * too. ei_qid changes with ei_update_lock held as well, so C reads it
     * under either.
     */
    #[pin]
    pub ei_quota: Locked<c::mutex, InodeQuota>,

    /*
     * When we've been doing nocow writes we'll need to issue flushes to the
     * underlying block devices
     *
     * XXX: a device may have had a flush issued by some other codepath. It
     * would be better to keep for each device a sequence number that's
     * incremented when we isusue a cache flush, and track here the sequence
     * number that needs flushing.
     */
    pub ei_devs_need_flush: c::AtomicDevsMask,

    /*
     * copy of inode in btree: written after a commit, the inode's btree
     * position still locked - which serializes its writers, not
     * ei_update_lock - and read without a lock
     */
    pub ei_inode: c::VolatileInodeUnpacked,

    #[pin]
    pub ei_writeback_timer: DelayedWork,
}
c_default!(InodeInfo);

// SAFETY: as c_default!()'s: all zeroes is valid - an inode's locks and timer
// are only zeroes until their init
unsafe impl ::pin_init::Zeroable for InodeInfo {}

#[bch_enum(closed)]
#[repr(u32)]
pub enum InodeInfoFlag {
    /*
     * A btree error on this inode: the VFS inode and the btree's may
     * disagree
     */
    EI_INODE_ERROR = 0,
    /* in a snapshot subvolume, which quotas don't count */
    EI_INODE_SNAPSHOT = 1,
    /*
     * set by a truncate of a nonempty file to 0: start writeback on
     * close - see DOC(replace-in-place) in vfs/file.rs
     */
    EI_INODE_FLUSH_ON_CLOSE = 2,
}

/*
 * Cached extent allocation state for [start, end), for skipping btree
 * lookups when initializing bch_folio state in sequential buffered
 * writes.
 *
 * Staleness contract: extents may change toward allocated without
 * notice (worst case we over-reserve); anything that deallocates must
 * clear the range, under pagecache_block so no fill (under
 * pagecache_add) can straddle the deallocation. Fillers build their
 * result in locals and publish it in one go: fillers aren't
 * serialized against each other (page_mkwrite fills without i_rwsem),
 * and incremental publishing would let two fills interleave into a
 * range neither of them scanned.
 */
#[repr(C)]
#[derive(MaybeZeroable, TypeInfo)]
pub struct InodeReserved {
    pub ei_reserved_replicas: u8,
    pub ei_reserved_state: u8,
}

published! {
    /// InodeReserved's range: written under ei_reserved_lock, and read without
    /// it by the buffered write path, to take the lock only for a folio in it.
    #[derive(MaybeZeroable)]
    pub struct InodeReservedRange {
        pub ei_reserved_start: Volatile<u64>,
        pub ei_reserved_end: Volatile<u64>,
    }
}

/// An inode's quota accounting, under ei_quota_lock.
#[repr(C)]
#[derive(MaybeZeroable, TypeInfo)]
pub struct InodeQuota {
    pub ei_qid: c::bch_qid,
    /// sectors reserved for writes, charged to ei_qid ahead of i_blocks
    pub ei_quota_reserved: u64,
}

/* returns 0 if we want to do the update, or error is passed up */
#[cfg(not(NO_BCACHEFS_FS))]
#[allow(non_camel_case_types)]
pub type inode_set_fn = Option<unsafe extern "C" fn(*mut BtreeTrans<'_>,
                                                    *mut c::InodeInfo,
                                                    *mut c::bch_inode_unpacked, *mut c_void)
                                                    -> c_int>;

/// ei_pagecache_lock's states: adding to the page cache, or blocking adds.
pub enum PagecacheAdd {}
pub enum PagecacheBlock {}

impl c::TwoState for PagecacheAdd {
    const S: bool = false;
}

impl c::TwoState for PagecacheBlock {
    const S: bool = true;
}
