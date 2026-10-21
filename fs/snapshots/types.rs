// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::locking::{new_mutex, Lock, Mutex};
use crate::new_lock;
use crate::types::{DArray, RustOnly, bits_to_longs, rust_only};
use crate::util::alloc::KVVec;
use crate::util::bitmap::AtomicBitArray;
use core::sync::atomic::{AtomicBool, AtomicU32};
use cstruct_macros::{c_const, c_enum, c_rcu_array, CStruct, c_typedef, Init, TypeInfo};
use pin_init::pin_data;

c_typedef! {
    pub type snapshot_id_list = DArray<u32>;
}

c_const! {
    #[c_int]
    pub const IS_ANCESTOR_BITMAP: u32 = 128;
}

/*
 * In-memory snapshot table entry, indexed by snapshot ID.
 *
 * Snapshots form a binary tree where IDs decrease going deeper: a parent's ID
 * is always greater than its children's.
 *
 * Ancestor lookups use a three-tier strategy:
 *  1. Skiplist (skip[]): jump up the tree in O(log n) steps
 *  2. Bitmap (is_ancestor[]): O(1) lookup for ancestors within 128 IDs
 *  3. Parent walk: fallback linear traversal
 *
 * Read under RCU; partial is_ancestor[] updates are tolerable since readers
 * fall back to the skiplist.
 */
c_enum! {
    #[closed]
    pub enum snapshot_id_state: u32 {
        SNAPSHOT_ID_empty,
        SNAPSHOT_ID_live,
        SNAPSHOT_ID_deleted,
    }
}

/*
 * A table entry: rewritten in place by mark_snapshot() (snapshot.rs) under
 * table_lock while readers read it under RCU alone, so every field is an
 * atomic - C still sees plain integers.
 */
#[repr(C)]
#[derive(CStruct)]
pub struct snapshot_t {
    #[c("enum snapshot_id_state state")]
    pub state: AtomicU32,
    #[c("u32 parent")]
    pub parent: AtomicU32,
    /* skiplist: random ancestors, sorted ascending; try [2] first */
    #[c("u32 skip[3]")]
    pub skip: [AtomicU32; 3],
    #[c("u32 depth")]
    pub depth: AtomicU32,
    #[c("u32 children[2]")]
    pub children: [AtomicU32; 2], /* normalized: [0] >= [1] */
    #[c("u32 subvol")]
    pub subvol: AtomicU32, /* Nonzero only if a subvolume points to this node: */
    #[c("u32 tree")]
    pub tree: AtomicU32,
    /* bit (ancestor - id - 1) set for ancestors within 128 IDs */
    #[c("unsigned long is_ancestor[BITS_TO_LONGS(IS_ANCESTOR_BITMAP)]")]
    pub is_ancestor: AtomicBitArray<{ bits_to_longs(c::IS_ANCESTOR_BITMAP as usize) }>,
}

// SAFETY: atomic integers; all zeroes is SNAPSHOT_ID_empty
unsafe impl crate::util::rcu::RcuElement for snapshot_t {}

c_rcu_array! {
    /// c->snapshots.table, published under RCU.
    pub type SnapshotTable = crate::util::rcu::RcuArray<c::snapshot_t>
        as struct snapshot_table { nr, s };
}

/*
 * Snapshot creation must prevent userspace from dirtying the page cache while
 * the snapshot is being taken: sync_inodes_sb flushes existing dirty pages
 * before the snapshot transaction, but if new pages get dirtied in the window
 * between sync_inodes_sb returning and the snapshot transaction running, those
 * dirty pages can be partially flushed (e.g. data page flushed but redo log
 * page not yet) such that the snapshot captures an inconsistent state — the
 * shape that bit MySQL/InnoDB.
 *
 * Page-cache dirtying paths (buffered write_iter and mmap mkdirty) take this
 * lock as readers; snapshot creation takes it as a writer. O_DIRECT doesn't
 * need it — direct writes commit as atomic btree transactions, no page cache
 * staleness window. Buffered writeback is fine too — each writeback insert
 * is atomic w.r.t. the snapshot transaction.
 */
#[pin_data]
#[repr(C)]
#[derive(CStruct, TypeInfo, Init)]
pub struct bch_fs_snapshots {
    pub table: c::SnapshotTable,
    /// The table's writers' lock: taken by table_write(), in snapshot.rs.
    #[pin]
    #[init(RustOnly::new(new_mutex!(())))]
    pub(crate) table_lock: rust_only!(Mutex<()>),
    /* a topology repair invalidated descendants' is_ancestor bitmaps: */
    #[c("bool need_table_rebuild")]
    #[init(AtomicBool::new(false))]
    pub(crate) need_table_rebuild: AtomicBool,
    #[c("struct percpu_rw_semaphore create_lock")]
    #[pin]
    #[init(new_lock!((), "&c->snapshots.create_lock"))]
    pub create_lock: Lock<c::percpu_rw_semaphore>,
    /// Snapshot deletion's state: Rust's alone - snapshots/delete.rs.
    #[pin]
    #[init(RustOnly::new(crate::snapshots::delete::SnapshotDelete::init()))]
    pub(crate) delete: rust_only!(crate::snapshots::delete::SnapshotDelete),
    #[c("struct work_struct wait_for_pagecache_and_delete_work")]
    #[pin]
    #[init(crate::snapshots::subvolume::SubvolDelete::init())]
    pub wait_for_pagecache_and_delete_work: crate::fs::FsWork,
    /// Subvolumes unlinked and waiting for the delete work: Rust's alone.
    #[pin]
    #[init(RustOnly::new(new_mutex!(KVVec::new())))]
    pub(crate) unlinked: rust_only!(Mutex<KVVec<u32>>),
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, CStruct)]
#[c_typedef]
pub struct subvol_inum {
    /* we can't have padding in this struct: */
    pub subvol: u64,
    pub inum: u64,
}
