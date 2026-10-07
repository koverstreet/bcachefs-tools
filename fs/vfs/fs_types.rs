// SPDX-License-Identifier: GPL-2.0

//! The data types of vfs/fs_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{c_enum, c_typedef, c_verbatim, CStruct};
use typeinfo_macros::TypeInfo;

#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_inode_info {
    pub v: c::inode,
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

    pub ei_inodes_idx: core::ffi::c_uint,
    pub ei_flags: core::ffi::c_ulong,

    pub ei_update_lock: c::mutex,
    pub ei_quota_reserved: u64,
    pub ei_last_dirtied: core::ffi::c_ulong,
    pub ei_pagecache_lock: c::two_state_lock_t,

    pub ei_quota_lock: c::mutex,
    pub ei_qid: c::bch_qid,

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

    /* copy of inode in btree: */
    pub ei_inode: c::bch_inode_unpacked,

    pub ei_writeback_timer: c::delayed_work,
}
c_default!(bch_inode_info);

c_enum! {
    #[flags]
    pub enum bch_inode_lock_op: u32 {
        INODE_PAGECACHE_BLOCK = 1 << 0,
        INODE_UPDATE_LOCK = 1 << 1,
    }
}

c_verbatim!(r#"
struct bch_inode_unpacked;
"#);

/* returns 0 if we want to do the update, or error is passed up */
#[cfg(not(NO_BCACHEFS_FS))]
c_typedef! {
    pub type inode_set_fn = Option<unsafe extern "C" fn(*mut c::btree_trans,
                                                        *mut c::bch_inode_info,
                                                        *mut c::bch_inode_unpacked, *mut core::ffi::c_void)
                                                        -> core::ffi::c_int>;
}
