// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

pub mod check_snapshots;
pub mod delete;
pub mod format;
pub mod snapshot;
pub mod subvolume;

use core::sync::atomic::AtomicBool;
use crate::fs::Fs;
use crate::locking::{Locked, Mutex, PercpuRwSem};
use crate::new_lock;
use crate::os::workqueue::ReffedWork;
use crate::snapshots::{delete::SnapshotDelete, snapshot::SnapshotTable, subvolume::delete_unlinked};
use crate::util::alloc::Vec;
use cstruct_macros::{placed, Init, MaybeZeroable, TypeInfo};
use pin_init::pin_data;

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
#[derive(MaybeZeroable, TypeInfo, Init)]
pub struct FsSnapshots {
    pub table: SnapshotTable,
    /// The table's writers' lock: taken by table_write(), in snapshot.rs.
    #[pin]
    #[init(new_lock!(()))]
    pub(crate) table_lock: Locked<Mutex>,
    /* a topology repair invalidated descendants' is_ancestor bitmaps: */
    #[init(AtomicBool::new(false))]
    pub(crate) need_table_rebuild: AtomicBool,
    #[pin]
    #[init(new_lock!((), "&c->snapshots.create_lock"))]
    pub create_lock: Locked<PercpuRwSem>,
    /// Snapshot deletion's state: Rust's alone - snapshots/delete.rs.
    #[pin]
    #[init(SnapshotDelete::init())]
    pub(crate) delete: SnapshotDelete,
    #[pin]
    pub(crate) wait_for_pagecache_and_delete_work: ReffedWork<delete_unlinked, crate::field!(super.writes)>,
    /// Subvolumes unlinked and waiting for the delete work: Rust's alone.
    #[pin]
    #[init(new_lock!(Vec::new()))]
    pub(crate) unlinked: Locked<Mutex, Vec<u32>>,
}

