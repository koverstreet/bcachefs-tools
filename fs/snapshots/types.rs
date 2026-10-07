// SPDX-License-Identifier: GPL-2.0

//! The data types of snapshots/types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{DArray, bits_to_longs, c_default};
use cstruct_macros::{c_const, c_enum, c_verbatim, CStruct};
use typeinfo_macros::TypeInfo;

c_verbatim!(r#"
DEFINE_DARRAY_NAMED(snapshot_id_list, u32);
"#);

pub type snapshot_id_list = DArray<u32>;

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
    pub is_ancestor: [core::ffi::c_ulong; bits_to_longs(c::IS_ANCESTOR_BITMAP as usize)],
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

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct snapshot_interior_delete {
    pub id: u32,
    pub live_child: u32,
}

c_verbatim!(r#"
DEFINE_DARRAY_NAMED(interior_delete_list, struct snapshot_interior_delete);
"#);

pub type interior_delete_list = DArray<c::snapshot_interior_delete>;

#[repr(C)]
#[derive(CStruct)]
pub struct snapshot_delete {
    pub progress_lock: c::mutex,
    pub deleting_from_trees: c::snapshot_id_list,
    pub delete_leaves: c::snapshot_id_list,
    pub delete_interior: c::interior_delete_list,
    pub no_keys: c::interior_delete_list,
    pub eytzinger_delete_list: c::interior_delete_list,

    pub running: bool,
    pub version: core::ffi::c_uint,
    pub progress: c::progress_indicator,
}
c_default!(snapshot_delete);

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
#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_fs_snapshots {
    #[c("struct snapshot_table __rcu *table")]
    pub table: *mut c::snapshot_table,
    pub table_lock: c::mutex,
    /* a topology repair invalidated descendants' is_ancestor bitmaps: */
    pub need_table_rebuild: bool,
    pub create_lock: c::percpu_rw_semaphore,
    pub delete: c::snapshot_delete,
    pub wait_for_pagecache_and_delete_work: c::work_struct,
    pub unlinked: c::snapshot_id_list,
    pub unlinked_lock: c::mutex,
}
c_default!(bch_fs_snapshots);

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, CStruct)]
#[c_typedef]
pub struct subvol_inum {
    /* we can't have padding in this struct: */
    pub subvol: u64,
    pub inum: u64,
}
