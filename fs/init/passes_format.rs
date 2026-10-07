// SPDX-License-Identifier: GPL-2.0

//! The data types of init/passes_format.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use core::ffi::c_ulong;
use zerocopy::byteorder::little_endian as le;
use cstruct_macros::{c_bitmask, c_const, c_enum, c_xmacro, CStruct};
use typeinfo_macros::TypeInfo;

c_const! {
    pub const PASS_SILENT: c_ulong = 1 << 0;
}

c_const! {
    pub const PASS_FSCK: c_ulong = 1 << 1;
}

c_const! {
    pub const PASS_UNCLEAN: c_ulong = 1 << 2;
}

c_const! {
    pub const PASS_ALWAYS: c_ulong = 1 << 3;
}

c_const! {
    pub const PASS_ONLINE: c_ulong = 1 << 4;
}

c_const! {
    pub const PASS_ALLOC: c_ulong = 1 << 5;
}

c_const! {
    pub const PASS_NODEFER: c_ulong = 1 << 6;
}

c_const! {
    pub const PASS_FSCK_ALLOC: c_ulong = c::PASS_FSCK | c::PASS_ALLOC;
}

#[cfg(CONFIG_BCACHEFS_DEBUG)]
c_const! {
    pub const PASS_FSCK_DEBUG: c_ulong = 1 << 1;
}

#[cfg(not(CONFIG_BCACHEFS_DEBUG))]
c_const! {
    #[c_int]
    pub const PASS_FSCK_DEBUG: c_ulong = 0;
}

c_xmacro! {
    /*
     * Passes may be reordered, but the second field is a persistent identifier and
     * must never change:
     */
    BCH_RECOVERY_PASSES(x) {
        (recovery_pass_empty,            41, PASS_SILENT,                            0,
         "Placeholder so scan_for_btree_nodes is not index 0"),
        (scan_for_btree_nodes,           37, 0,                                      0,
         "Scan all devices for btree nodes by magic number, "
         "deduplicate replicas, and build node scan table "
         "for topology repair"),
        (check_topology,                  4, 0,
         BIT_ULL(BCH_RECOVERY_PASS_scan_for_btree_nodes),
         "Verify btree roots exist (reconstructing from node "
         "scan if missing), then recursively validate "
         "parent-child links and min/max key boundaries"),
        (accounting_read,                39, PASS_ALWAYS,
         BIT_ULL(BCH_RECOVERY_PASS_check_topology),
         "Read accounting keys from btree and journal into "
         "memory, merging deltas and initializing per-device "
         "usage counters"),
        (alloc_read,                      0, PASS_ALWAYS,                            0,
         "Populate in-memory bucket generation cache from "
         "bucket_gens btree (or alloc btree on older "
         "filesystems)"),
        (stripes_read,                    1, 0,                                      0,
         "Reserved for erasure-coding stripe initialization; "
         "currently a no-op"),
        (initialize_subvolumes,           2, 0,                                      0,
         "Create root snapshot tree, root snapshot node, "
         "and root subvolume for a new filesystem"),
        (snapshots_read,                  3, PASS_ALWAYS,                            0,
         "Populate in-memory snapshot table with ancestry "
         "bitmaps and depth info by iterating snapshot "
         "btree in reverse order"),
        (check_allocations,               5, PASS_FSCK_ALLOC,
         BIT_ULL(BCH_RECOVERY_PASS_check_topology),
         "Full GC pass: walk all btrees marking referenced "
         "buckets, then compare against alloc btree to "
         "repair data_type, sector counts, and stripe refs"),
        (trans_mark_dev_sbs,              6, PASS_ALWAYS | PASS_SILENT | PASS_ALLOC, 0,
         "Mark superblock and journal regions in alloc btree"),
        (fs_journal_alloc,                7, PASS_ALWAYS | PASS_SILENT | PASS_ALLOC, 0,
         "Ensure journal has allocated buckets"),
        (set_may_go_rw,                   8, PASS_ALWAYS | PASS_SILENT,
         BIT_ULL(BCH_RECOVERY_PASS_check_allocations),
         "Enable read-write mode; btree updates go to "
         "journal instead of replay buffer"),
        (journal_replay,                  9, PASS_ALWAYS,
         BIT_ULL(BCH_RECOVERY_PASS_set_may_go_rw),
         "Replay pending journal keys into btrees, "
         "accounting keys first; sorted-order bulk insert "
         "with per-key fallback for journal deadlocks"),
        (merge_btree_nodes,              45, PASS_ONLINE,                            0,
         "Merge adjacent underfull btree nodes to reclaim "
         "wasted space"),
        (presplit_shard_boundaries,      48, PASS_ALWAYS,
         BIT_ULL(BCH_RECOVERY_PASS_journal_replay),
         "Split btree leaves spanning inode-allocator shard "
         "boundaries so each shard's keys live in dedicated "
         "nodes (cache locality for sharded inode/dirent/"
         "extent/xattr access)"),
        (check_alloc_info,               10, PASS_ONLINE | PASS_FSCK_ALLOC,
         BIT_ULL(BCH_RECOVERY_PASS_check_allocations),
         "Cross-check alloc btree against freespace, "
         "need_discard, and bucket_gens btrees; repair "
         "missing or incorrect entries in each"),
        (check_lrus,                     11, PASS_ONLINE | PASS_FSCK_ALLOC,
         BIT_ULL(BCH_RECOVERY_PASS_check_allocations),
         "Verify LRU btree entries match alloc key "
         "timestamps for cached-data and fragmentation "
         "LRUs; delete stale entries"),
        (check_btree_backpointers,       12, PASS_ONLINE | PASS_FSCK_ALLOC,
         BIT_ULL(BCH_RECOVERY_PASS_check_allocations),
         "Verify every backpointer entry references a "
         "valid alloc key; remove backpointers for "
         "nonexistent buckets"),
        (check_backpointers_to_extents,  13, PASS_ONLINE,
         BIT_ULL(BCH_RECOVERY_PASS_check_allocations),
         "Verify each backpointer matches an actual extent "
         "or btree pointer at the claimed location; "
         "remove stale entries"),
        (check_extents_to_backpointers,  14, PASS_ONLINE | PASS_FSCK_ALLOC,
         BIT_ULL(BCH_RECOVERY_PASS_check_allocations),
         "Find buckets with missing backpointers by "
         "scanning alloc btree, then regenerate them "
         "from extent and btree pointer data"),
        (check_alloc_to_lru_refs,        15, PASS_ONLINE | PASS_FSCK_ALLOC,
         BIT_ULL(BCH_RECOVERY_PASS_check_allocations),
         "Ensure cached buckets have correct cached-data "
         "LRU entries and fragmentable buckets have correct "
         "fragmentation LRU entries"),
        (fs_freespace_init,              16, PASS_ALWAYS | PASS_SILENT,              0,
         "Initialize freespace btree from alloc info"),
        (bucket_gens_init,               17, 0,                                      0,
         "Populate bucket_gens btree from alloc btree "
         "generation numbers; one-time migration"),
        (reconstruct_snapshots,          38, 0,                                      0,
         "Scan snapshot-bearing btrees to find snapshot IDs "
         "in use, then reconstruct missing snapshot nodes "
         "and tree entries"),
        (delete_dead_interior_snapshots, 44, 0,                                      0,
         "Collapse interior snapshot nodes with no remaining "
         "keys by re-parenting their single live child"),
        (check_snapshot_trees,           18, PASS_ONLINE | PASS_FSCK,
         BIT_ULL(BCH_RECOVERY_PASS_reconstruct_snapshots),
         "Validate snapshot_tree entries: root snapshot "
         "reference, back-link consistency, master_subvol "
         "points to a real non-snapshot subvolume"),
        (check_snapshots,                19, PASS_ALWAYS | PASS_ONLINE | PASS_FSCK | PASS_NODEFER,
         BIT_ULL(BCH_RECOVERY_PASS_reconstruct_snapshots) |
         BIT_ULL(BCH_RECOVERY_PASS_check_snapshot_trees),
         "Validate snapshot btree in reverse order: "
         "parent/child bidirectional links, tree_id, "
         "depth, subvol flag, and skiplist pointers"),
        (resume_logged_ops_early,        50, PASS_ALWAYS,                            0,
         "Resume incomplete logged operations that background "
         "work can start (stripe creation), before copygc and "
         "reconcile may run"),
        (check_subvols,                  20, PASS_ONLINE | PASS_FSCK,
         BIT_ULL(BCH_RECOVERY_PASS_check_snapshots),
         "Validate subvolume entries: snapshot exists, "
         "root inode has correct bi_subvol, "
         "fs_path_parent is valid; delete unlinked subvols"),
        (check_subvol_children,          35, PASS_ONLINE | PASS_FSCK,
         BIT_ULL(BCH_RECOVERY_PASS_check_subvols),
         "Walk subvolume_children btree and remove entries "
         "not matching a real subvolume with correct "
         "fs_path_parent"),
        (delete_dead_snapshots,          21, PASS_ONLINE | PASS_FSCK,
         BIT_ULL(BCH_RECOVERY_PASS_check_snapshots),
         "Delete snapshot data keys across all "
         "snapshot-bearing btrees, then remove snapshot "
         "nodes and mark empty interior nodes"),
        (fs_upgrade_for_subvolumes,      22, 0,                                      0,
         "One-time migration: set bi_subvol on root inode "
         "for pre-subvolumes filesystems"),
        (check_inodes,                   24, PASS_FSCK,
         BIT_ULL(BCH_RECOVERY_PASS_check_snapshots) |
         BIT_ULL(BCH_RECOVERY_PASS_delete_dead_snapshots),
         "Validate inode fields (mode, flags, i_size, "
         "bi_subvol), delete orphaned unlinked inodes, "
         "repair invalid backpointers"),
        (check_extents,                  25, PASS_FSCK,
         BIT_ULL(BCH_RECOVERY_PASS_check_inodes) |
         BIT_ULL(BCH_RECOVERY_PASS_delete_dead_snapshots),
         "Validate extent keys: owning inode exists, "
         "snapshot valid, no overlaps, i_size and "
         "i_sectors consistent"),
        (check_indirect_extents,         26, PASS_ONLINE | PASS_FSCK,
         BIT_ULL(BCH_RECOVERY_PASS_check_snapshots),
         "Validate reflink indirect extents; drop stale "
         "device pointers whose generation no longer "
         "matches"),
        (check_dirents,                  27, PASS_FSCK,
         BIT_ULL(BCH_RECOVERY_PASS_check_inodes) |
         BIT_ULL(BCH_RECOVERY_PASS_delete_dead_snapshots),
         "Validate directory entries: target inode exists "
         "in correct snapshot, d_type matches inode mode, "
         "hash values correct"),
        (check_xattrs,                   28, PASS_FSCK,
         BIT_ULL(BCH_RECOVERY_PASS_check_inodes) |
         BIT_ULL(BCH_RECOVERY_PASS_delete_dead_snapshots),
         "Validate xattr entries: owning inode exists "
         "in valid snapshot, hash correct; delete orphans"),
        (check_damage,                   49, PASS_FSCK,
         BIT_ULL(BCH_RECOVERY_PASS_check_inodes),
         "Delete damage keys with no inode at the "
         "same position - left behind by kernels "
         "without the damage btree"),
        (check_root,                     29, PASS_ONLINE | PASS_FSCK,
         BIT_ULL(BCH_RECOVERY_PASS_check_inodes),
         "Ensure root subvolume and root directory inode "
         "exist; create them if missing"),
        (check_unreachable_inodes,       40, PASS_FSCK,
         BIT_ULL(BCH_RECOVERY_PASS_check_inodes) |
         BIT_ULL(BCH_RECOVERY_PASS_check_dirents),
         "Find inodes with no directory entry (unset "
         "bi_dir backpointer); reattach to lost+found"),
        (check_subvolume_structure,      36, PASS_ONLINE | PASS_FSCK,
         BIT_ULL(BCH_RECOVERY_PASS_check_subvols) |
         BIT_ULL(BCH_RECOVERY_PASS_check_inodes),
         "Follow each subvolume's fs_path_parent chain "
         "to root, verify no cycles or dead ends; "
         "reattach disconnected subvolumes"),
        (check_directory_structure,      30, PASS_ONLINE | PASS_FSCK,
         BIT_ULL(BCH_RECOVERY_PASS_check_unreachable_inodes),
         "DFS from each directory following parent "
         "pointers, detect cycles, renumber bi_depth, "
         "reattach disconnected directories"),
        (check_nlinks,                   31, PASS_FSCK,
         BIT_ULL(BCH_RECOVERY_PASS_check_inodes) |
         BIT_ULL(BCH_RECOVERY_PASS_check_dirents),
         "Two-pass nlink verification: collect hardlinked "
         "inodes, count directory references, fix "
         "bi_nlink mismatches"),
        (check_reconcile_work,           43, PASS_ONLINE | PASS_FSCK,
         BIT_ULL(BCH_RECOVERY_PASS_check_snapshots),
         "Validate reconcile work/hipri/pending/scan "
         "btrees against actual extent data; remove "
         "stale entries"),
        (resume_logged_ops,              23, PASS_ALWAYS,                            0,
         "Resume incomplete logged operations only userspace "
         "starts (truncate, finsert, option propagation) from "
         "logged_ops btree, then delete completed entries"),
        (delete_dead_inodes,             32, PASS_ALWAYS,                            0,
         "Scan deleted_inodes btree and fully remove "
         "inodes with nlink == 0 that are not open"),
        (kill_i_generation_keys,         47, PASS_ONLINE,                            0,
         "Remove KEY_TYPE_inode_generation keys; this older "
         "generation persistence mechanism has been "
         "superseded"),
        (fix_reflink_p,                  33, 0,                                      0,
         "One-time migration: clear stale front_pad/"
         "back_pad fields in KEY_TYPE_reflink_p keys"),
        (set_fs_needs_reconcile,         34, 0,                                      0,
         "One-time pass: insert full-filesystem "
         "reconcile_scan entry to trigger background "
         "data verification"),
        (btree_bitmap_gc,                46, PASS_ONLINE,                            0,
         "Recompute per-device btree_allocated_bitmap "
         "by scanning all live btree node pointers"),
        (lookup_root_inode,              42, PASS_ALWAYS | PASS_SILENT,              0,
         "Verify root inode is readable before "
         "completing recovery"),
    }
}

/* We normally enumerate recovery passes in the order we run them: */
macro_rules! __bch_recovery_pass_0 {
    ([$($acc:tt)*] $(($n:tt$(, $($__rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[closed]
            pub enum bch_recovery_pass: u32 {
                $($acc)*
                $([<BCH_RECOVERY_PASS_ $n>],)*
                BCH_RECOVERY_PASS_NR,
            }
        }
    } };
}
BCH_RECOVERY_PASSES!(__bch_recovery_pass_0 []);

/* But we also need stable identifiers that can be used in the superblock */
macro_rules! __bch_recovery_pass_stable_0 {
    ([$($acc:tt)*] $(($n:tt, $id:expr$(, $($__rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_recovery_pass_stable: u32 {
                $($acc)*
                $([<BCH_RECOVERY_PASS_STABLE_ $n>] = (($id) as u32),)*
            }
        }
    } };
}
BCH_RECOVERY_PASSES!(__bch_recovery_pass_stable_0 []);

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct)]
pub struct recovery_pass_entry {
    pub last_run: le::U64,
    pub last_runtime: le::U32,
    pub flags: le::U32,
}

c_bitmask! {
    LE32_BITMASK(struct recovery_pass_entry, flags) {
        BCH_RECOVERY_PASS_NO_RATELIMIT(0, 1),
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_sb_field_recovery_passes {
    pub field: c::bch_sb_field,
    pub start: [c::recovery_pass_entry; 0],
}
