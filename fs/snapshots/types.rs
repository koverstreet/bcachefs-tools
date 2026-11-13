// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::fs::Fs;
use crate::locking::Locked;
use crate::new_lock;
use crate::snapshots::subvolume::delete_unlinked;
use crate::types::{bits_to_longs};
use crate::util::alloc::KVVec;
use crate::data_structures::bitmap::AtomicBitArray;
use crate::util::workqueue::ReffedWork;
use core::sync::atomic::{AtomicBool, AtomicU32};
use cstruct_macros::{bch_enum, placed, CStruct, Init, TypeInfo};
use pin_init::pin_data;

pub const IS_ANCESTOR_BITMAP: u32 = 128;

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
#[bch_enum(closed)]
#[repr(u32)]
pub enum snapshot_id_state {
    SNAPSHOT_ID_empty,
    SNAPSHOT_ID_live,
    SNAPSHOT_ID_deleted,
}

/*
 * A table entry: rewritten in place by mark_snapshot() (snapshot.rs) under
 * table_lock while readers read it under RCU alone, so every field is an
 * atomic - C still sees plain integers.
 */
#[repr(C)]
#[derive(CStruct)]
pub struct snapshot_t {
    pub state: AtomicU32,
    pub parent: AtomicU32,
    /* skiplist: random ancestors, sorted ascending; try [2] first */
    pub skip: [AtomicU32; 3],
    pub depth: AtomicU32,
    pub children: [AtomicU32; 2], /* normalized: [0] >= [1] */
    pub subvol: AtomicU32, /* Nonzero only if a subvolume points to this node: */
    pub tree: AtomicU32,
    /* bit (ancestor - id - 1) set for ancestors within 128 IDs */
    pub is_ancestor: AtomicBitArray<{ bits_to_longs(c::IS_ANCESTOR_BITMAP as usize) }>,
}

// SAFETY: atomic integers; all zeroes is SNAPSHOT_ID_empty
unsafe impl crate::util::rcu::RcuElement for snapshot_t {}

/// c->snapshots.table, published under RCU.
pub type SnapshotTable = crate::util::rcu::RcuArray<c::snapshot_t>;

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
#[placed(Fs.snapshots)]
#[pin_data]
#[repr(C)]
#[derive(CStruct, TypeInfo, Init)]
pub struct bch_fs_snapshots {
    pub table: c::SnapshotTable,
    /// The table's writers' lock: taken by table_write(), in snapshot.rs.
    #[pin]
    #[init(new_lock!(()))]
    pub(crate) table_lock: Locked<c::mutex>,
    /* a topology repair invalidated descendants' is_ancestor bitmaps: */
    #[init(AtomicBool::new(false))]
    pub(crate) need_table_rebuild: AtomicBool,
    #[pin]
    #[init(new_lock!((), "&c->snapshots.create_lock"))]
    pub create_lock: Locked<c::percpu_rw_semaphore>,
    /// Snapshot deletion's state: Rust's alone - snapshots/delete.rs.
    #[pin]
    #[init(crate::snapshots::delete::SnapshotDelete::init())]
    pub(crate) delete: crate::snapshots::delete::SnapshotDelete,
    #[pin]
    pub(crate) wait_for_pagecache_and_delete_work: ReffedWork<delete_unlinked, crate::field!(super.writes)>,
    /// Subvolumes unlinked and waiting for the delete work: Rust's alone.
    #[pin]
    #[init(new_lock!(KVVec::new()))]
    pub(crate) unlinked: Locked<c::mutex, KVVec<u32>>,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, CStruct)]
pub struct subvol_inum {
    /* we can't have padding in this struct: */
    pub subvol: u64,
    pub inum: u64,
}

impl subvol_inum {
    /// The root directory, in the root subvolume.
    pub const ROOT: Self = Self { subvol: c::BCACHEFS_ROOT_SUBVOL as u64, inum: c::BCACHEFS_ROOT_INO as u64 };
}
