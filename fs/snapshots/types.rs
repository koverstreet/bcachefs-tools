// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{DArray, RustOnly, bits_to_longs, c_default, rust_only};
use crate::util::alloc::KVVec;
use crate::util::sync::{new_mutex, Mutex};
use core::ffi::c_ulong;
use cstruct_macros::{c_const, c_enum, CStruct, c_typedef, Init, TypeInfo};
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

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct snapshot_t {
    pub state: c::snapshot_id_state,
    pub parent: u32,
    /* skiplist: random ancestors, sorted ascending; try [2] first */
    pub skip: [u32; 3],
    pub depth: u32,
    pub children: [u32; 2], /* normalized: [0] >= [1] */
    pub subvol: u32, /* Nonzero only if a subvolume points to this node: */
    pub tree: u32,
    /* bit (ancestor - id - 1) set for ancestors within 128 IDs */
    #[c("unsigned long is_ancestor[BITS_TO_LONGS(IS_ANCESTOR_BITMAP)]")]
    pub is_ancestor: [c_ulong; bits_to_longs(c::IS_ANCESTOR_BITMAP as usize)],
}
c_default!(snapshot_t);

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct snapshot_table {
    pub rcu: c::rcu_head,
    pub nr: usize,
    #[c("DECLARE_FLEX_ARRAY(struct snapshot_t, s)")]
    pub s: [c::snapshot_t; 0],
}
c_default!(snapshot_table);

// SAFETY: Copy, of integers and a snapshot_id_state, whose 0 is empty
unsafe impl crate::util::rcu::RcuElement for snapshot_t {}

// C's view of the table bch_fs_snapshots.table is an RcuArray of
const _: () = {
    use crate::util::rcu::RcuArrayBuf;
    use core::mem::offset_of;
    assert!(offset_of!(snapshot_table, rcu) == 0);
    assert!(offset_of!(snapshot_table, nr) == core::mem::size_of::<c::rcu_head>());
    assert!(offset_of!(snapshot_table, s) == RcuArrayBuf::<c::snapshot_t>::TAIL);
};

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
    #[c("struct snapshot_table __rcu *table")]
    pub table: crate::util::rcu::RcuArray<c::snapshot_t>,
    pub table_lock: c::mutex,
    /* a topology repair invalidated descendants' is_ancestor bitmaps: */
    pub need_table_rebuild: bool,
    pub create_lock: c::percpu_rw_semaphore,
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
