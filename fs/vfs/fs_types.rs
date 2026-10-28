// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::locking::Lock;
use crate::types::c_default;
use crate::util::bitmap::AtomicULong;
use crate::vfs::vfs_inode::VfsInode;
use core::ffi::{c_uint, c_ulong};
#[cfg(not(NO_BCACHEFS_FS))]
use core::ffi::{c_int, c_void};
use cstruct_macros::{c_enum, CStruct, TypeInfo};
#[cfg(not(NO_BCACHEFS_FS))]
use cstruct_macros::{c_extern, c_typedef};

/*
 * The VFS's inode, and what we keep beside it. Shared - the kernel and C use it
 * while Rust has a &InodeInfo - so what Rust touches is typed for that: @v,
 * the struct inode the kernel writes, a VfsInode; ei_inode a
 * VolatileInodeUnpacked; ei_update_lock C's mutex, borrowed as a Lock;
 * ei_quota_lock a Lock over what it protects.
 */
#[repr(C)]
#[derive(CStruct, TypeInfo)]
#[c_name(bch_inode_info)]
pub struct InodeInfo {
    #[c("struct inode v")]
    pub v: VfsInode,
    pub hash: c::rhash_head,
    /*
     * Read it with inode_inum(); the layout is private so that
     * bch2_inode_or_descendents_is_open() can walk entries without knowing
     * about bch_inode_info at all.
     */
    pub ei_inum_hash: c::bch_inum_hash_entry,

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
    pub ei_reserved_start: u64,
    pub ei_reserved_end: u64,
    pub ei_reserved_replicas: u8,
    pub ei_reserved_state: u8,
    pub ei_reserved_lock: c::spinlock_t,

    pub ei_inodes_idx: c_uint,
    /// bch_inode_info_flags, by bit: C's set_bit() and test_bit()'s, so atomic
    pub ei_flags: AtomicULong,

    pub ei_update_lock: c::mutex,
    pub ei_last_dirtied: c_ulong,
    pub ei_pagecache_lock: c::two_state_lock_t,

    /*
     * The quota the inode's usage is charged to: i_blocks + ei_quota_reserved
     * is charged to ei_qid. i_blocks, the VFS inode's, changes under the lock
     * too. ei_qid changes with ei_update_lock held as well, so C reads it
     * under either.
     */
    #[c_lock("struct mutex ei_quota_lock")]
    pub ei_quota: Lock<c::mutex, InodeQuota>,

    /*
     * When we've been doing nocow writes we'll need to issue flushes to the
     * underlying block devices
     *
     * XXX: a device may have had a flush issued by some other codepath. It
     * would be better to keep for each device a sequence number that's
     * incremented when we isusue a cache flush, and track here the sequence
     * number that needs flushing.
     */
    pub ei_devs_need_flush: c::bch_devs_mask,

    /*
     * copy of inode in btree: written after a commit, the inode's btree
     * position still locked - which serializes its writers, not
     * ei_update_lock - and read without a lock
     */
    #[c("struct bch_inode_unpacked ei_inode")]
    pub ei_inode: c::VolatileInodeUnpacked,

    pub ei_writeback_timer: c::delayed_work,
}
c_default!(InodeInfo);

/// C's name, as FFI declarations spell it.
pub type bch_inode_info = InodeInfo;

c_enum! {
    #[closed]
    pub enum bch_inode_info_flags: u32 {
        /*
         * A btree error on this inode: the VFS inode and the btree's may
         * disagree
         */
        EI_INODE_ERROR = 0,
        /* in a snapshot subvolume, which quotas don't count */
        EI_INODE_SNAPSHOT = 1,
        /*
         * in the inode cache: tested by a lookup and cleared by removal under
         * i_lock; set before the insert, and cleared if it loses the race,
         * while the inode is ours alone
         */
        EI_INODE_HASHED = 2,
        /*
         * set by a truncate of a nonempty file to 0: start writeback on
         * close - see DOC(replace-in-place) in fs.c
         */
        EI_INODE_FLUSH_ON_CLOSE = 3,
    }
}

/// An inode's quota accounting, under ei_quota_lock.
#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct InodeQuota {
    pub ei_qid: c::bch_qid,
    /// sectors reserved for writes, charged to ei_qid ahead of i_blocks
    pub ei_quota_reserved: u64,
}

c_enum! {
    #[flags]
    pub enum bch_inode_lock_op: u32 {
        INODE_PAGECACHE_BLOCK = 1 << 0,
        INODE_UPDATE_LOCK = 1 << 1,
    }
}

/* returns 0 if we want to do the update, or error is passed up */
#[cfg(not(NO_BCACHEFS_FS))]
c_typedef! {
    pub type inode_set_fn = Option<unsafe extern "C" fn(*mut c::btree_trans,
                                                        *mut c::bch_inode_info,
                                                        *mut c::bch_inode_unpacked, *mut c_void)
                                                        -> c_int>;
}

#[cfg(not(NO_BCACHEFS_FS))]
c_extern! {
    pub fn bch2_inode_update_after_write(trans: *mut c::btree_trans, ei: *mut c::bch_inode_info,
                                         bi: *mut c::bch_inode_unpacked, fields: c_uint);
    #[must_use]
    pub fn bch2_write_inode(c: *mut c::bch_fs, ei: *mut c::bch_inode_info, set: c::inode_set_fn,
                            p: *mut c_void, fields: c_uint) -> c_int;
    pub fn bch2_inode_or_descendents_is_open(trans: *mut c::btree_trans, pos: c::bpos) -> c_int;
    pub fn bch2_evict_subvolume_inodes(c: *mut c::bch_fs, s: *mut c::snapshot_id_list);
}
