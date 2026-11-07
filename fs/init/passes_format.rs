// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{Le32, Le64};
use crate::util::vstructs::{vstruct, Head};
use cstruct_macros::{c_bitmask, c_enum, unsized_vstruct, CStruct, Plain, TypeInfo};

c_enum! {
    #[flags]
    #[c_name(bch_recovery_pass_flags)]
    #[prefix(PASS_)]
    pub enum PassFlags: u32 {
        PASS_SILENT = 1 << 0,
        PASS_FSCK = 1 << 1,
        PASS_UNCLEAN = 1 << 2,
        PASS_ALWAYS = 1 << 3,
        PASS_ONLINE = 1 << 4,
        PASS_ALLOC = 1 << 5,
        PASS_NODEFER = 1 << 6,
    }
}

/*
 * Passes may be reordered, but a pass's #[stable] id is persistent, and must
 * never change:
 */
c_enum! {
    #[closed]
    #[c_name(bch_recovery_pass)]
    #[prefix(BCH_RECOVERY_PASS_)]
    #[nr(BCH_RECOVERY_PASS_NR)]
    #[arm_columns(stable: enum(RecoveryPassStable: u32, prefix = BCH_RECOVERY_PASS_STABLE_,
                               c_name = bch_recovery_pass_stable),
                  when: PassFlags, depends: set(RecoveryPass))]
    pub enum RecoveryPass: u32 {
        /// Placeholder so scan_for_btree_nodes is not index 0
        #[stable(41)] #[when(SILENT)]
        recovery_pass_empty,
        /// Scan all devices for btree nodes by magic number, deduplicate
        /// replicas, and build node scan table for topology repair
        #[stable(37)]
        scan_for_btree_nodes,
        /// Verify btree roots exist (reconstructing from node scan if missing),
        /// then recursively validate parent-child links and min/max key
        /// boundaries
        #[stable(4)]
        #[depends(scan_for_btree_nodes)]
        check_topology,
        /// Read accounting keys from btree and journal into memory, merging
        /// deltas and initializing per-device usage counters
        #[stable(39)] #[when(ALWAYS)]
        #[depends(check_topology)]
        accounting_read,
        /// Populate in-memory bucket generation cache from bucket_gens btree
        /// (or alloc btree on older filesystems)
        #[stable(0)] #[when(ALWAYS)]
        alloc_read,
        /// Reserved for erasure-coding stripe initialization; currently a no-op
        #[stable(1)]
        stripes_read,
        /// Create root snapshot tree, root snapshot node, and root subvolume
        /// for a new filesystem
        #[stable(2)]
        initialize_subvolumes,
        /// Populate in-memory snapshot table with ancestry bitmaps and depth
        /// info by iterating snapshot btree in reverse order
        #[stable(3)] #[when(ALWAYS)]
        snapshots_read,
        /// Full GC pass: walk all btrees marking referenced buckets, then
        /// compare against alloc btree to repair data_type, sector counts, and
        /// stripe refs
        #[stable(5)] #[when(FSCK, ALLOC)]
        #[depends(check_topology)]
        check_allocations,
        /// Mark superblock and journal regions in alloc btree
        #[stable(6)] #[when(ALWAYS, SILENT, ALLOC)]
        trans_mark_dev_sbs,
        /// Ensure journal has allocated buckets
        #[stable(7)] #[when(ALWAYS, SILENT, ALLOC)]
        fs_journal_alloc,
        /// Enable read-write mode; btree updates go to journal instead of
        /// replay buffer
        #[stable(8)] #[when(ALWAYS, SILENT)]
        #[depends(check_allocations)]
        set_may_go_rw,
        /// Replay pending journal keys into btrees, accounting keys first;
        /// sorted-order bulk insert with per-key fallback for journal deadlocks
        #[stable(9)] #[when(ALWAYS)]
        #[depends(set_may_go_rw)]
        journal_replay,
        /// Merge adjacent underfull btree nodes to reclaim wasted space
        #[stable(45)] #[when(ONLINE)]
        merge_btree_nodes,
        /// Split btree leaves spanning inode-allocator shard boundaries so each
        /// shard's keys live in dedicated nodes (cache locality for sharded
        /// inode/dirent/extent/xattr access)
        #[stable(48)] #[when(ALWAYS)]
        #[depends(journal_replay)]
        presplit_shard_boundaries,
        /// Cross-check alloc btree against freespace, need_discard, and
        /// bucket_gens btrees; repair missing or incorrect entries in each
        #[stable(10)] #[when(ONLINE, FSCK, ALLOC)]
        #[depends(check_allocations)]
        check_alloc_info,
        /// Verify LRU btree entries match alloc key timestamps for cached-data
        /// and fragmentation LRUs; delete stale entries
        #[stable(11)] #[when(ONLINE, FSCK, ALLOC)]
        #[depends(check_allocations)]
        check_lrus,
        /// Verify every backpointer entry references a valid alloc key; remove
        /// backpointers for nonexistent buckets
        #[stable(12)] #[when(ONLINE, FSCK, ALLOC)]
        #[depends(check_allocations)]
        check_btree_backpointers,
        /// Verify each backpointer matches an actual extent or btree pointer at
        /// the claimed location; remove stale entries
        #[stable(13)] #[when(ONLINE)]
        #[depends(check_allocations)]
        check_backpointers_to_extents,
        /// Find buckets with missing backpointers by scanning alloc btree, then
        /// regenerate them from extent and btree pointer data
        #[stable(14)] #[when(ONLINE, FSCK, ALLOC)]
        #[depends(check_allocations)]
        check_extents_to_backpointers,
        /// Ensure cached buckets have correct cached-data LRU entries and
        /// fragmentable buckets have correct fragmentation LRU entries
        #[stable(15)] #[when(ONLINE, FSCK, ALLOC)]
        #[depends(check_allocations)]
        check_alloc_to_lru_refs,
        /// Initialize freespace btree from alloc info
        #[stable(16)] #[when(ALWAYS, SILENT)]
        fs_freespace_init,
        /// Populate bucket_gens btree from alloc btree generation numbers; one-
        /// time migration
        #[stable(17)]
        bucket_gens_init,
        /// Scan snapshot-bearing btrees to find snapshot IDs in use, then
        /// reconstruct missing snapshot nodes and tree entries
        #[stable(38)]
        reconstruct_snapshots,
        /// Collapse interior snapshot nodes with no remaining keys by re-
        /// parenting their single live child
        #[stable(44)]
        delete_dead_interior_snapshots,
        /// Validate snapshot_tree entries: root snapshot reference, back-link
        /// consistency, master_subvol points to a real non-snapshot subvolume
        #[stable(18)] #[when(ONLINE, FSCK)]
        #[depends(reconstruct_snapshots)]
        check_snapshot_trees,
        /// Validate snapshot btree in reverse order: parent/child bidirectional
        /// links, tree_id, depth, subvol flag, and skiplist pointers
        #[stable(19)] #[when(ALWAYS, ONLINE, FSCK, NODEFER)]
        #[depends(reconstruct_snapshots, check_snapshot_trees)]
        check_snapshots,
        /// Resume incomplete logged operations that background work can start
        /// (stripe creation), before copygc and reconcile may run
        #[stable(50)] #[when(ALWAYS)]
        resume_logged_ops_early,
        /// Validate subvolume entries: snapshot exists, root inode has correct
        /// bi_subvol, fs_path_parent is valid; delete unlinked subvols
        #[stable(20)] #[when(ONLINE, FSCK)]
        #[depends(check_snapshots)]
        check_subvols,
        /// Walk subvolume_children btree and remove entries not matching a real
        /// subvolume with correct fs_path_parent
        #[stable(35)] #[when(ONLINE, FSCK)]
        #[depends(check_subvols)]
        check_subvol_children,
        /// Delete snapshot data keys across all snapshot-bearing btrees, then
        /// remove snapshot nodes and mark empty interior nodes
        #[stable(21)] #[when(ONLINE, FSCK)]
        #[depends(check_snapshots)]
        delete_dead_snapshots,
        /// One-time migration: set bi_subvol on root inode for pre-subvolumes
        /// filesystems
        #[stable(22)]
        fs_upgrade_for_subvolumes,
        /// Validate inode fields (mode, flags, i_size, bi_subvol), delete
        /// orphaned unlinked inodes, repair invalid backpointers
        #[stable(24)] #[when(FSCK)]
        #[depends(check_snapshots, delete_dead_snapshots)]
        check_inodes,
        /// Validate extent keys: owning inode exists, snapshot valid, no
        /// overlaps, i_size and i_sectors consistent
        #[stable(25)] #[when(FSCK)]
        #[depends(check_inodes, delete_dead_snapshots)]
        check_extents,
        /// Validate reflink indirect extents; drop stale device pointers whose
        /// generation no longer matches
        #[stable(26)] #[when(ONLINE, FSCK)]
        #[depends(check_snapshots)]
        check_indirect_extents,
        /// Validate directory entries: target inode exists in correct snapshot,
        /// d_type matches inode mode, hash values correct
        #[stable(27)] #[when(FSCK)]
        #[depends(check_inodes, delete_dead_snapshots)]
        check_dirents,
        /// Validate xattr entries: owning inode exists in valid snapshot, hash
        /// correct; delete orphans
        #[stable(28)] #[when(FSCK)]
        #[depends(check_inodes, delete_dead_snapshots)]
        check_xattrs,
        /// Delete damage keys with no inode at the same position - left behind
        /// by kernels without the damage btree
        #[stable(49)] #[when(FSCK)]
        #[depends(check_inodes)]
        check_damage,
        /// Ensure root subvolume and root directory inode exist; create them if
        /// missing
        #[stable(29)] #[when(ONLINE, FSCK)]
        #[depends(check_inodes)]
        check_root,
        /// Find inodes with no directory entry (unset bi_dir backpointer);
        /// reattach to lost+found
        #[stable(40)] #[when(FSCK)]
        #[depends(check_inodes, check_dirents)]
        check_unreachable_inodes,
        /// Follow each subvolume's fs_path_parent chain to root, verify no
        /// cycles or dead ends; reattach disconnected subvolumes
        #[stable(36)] #[when(ONLINE, FSCK)]
        #[depends(check_subvols, check_inodes)]
        check_subvolume_structure,
        /// DFS from each directory following parent pointers, detect cycles,
        /// renumber bi_depth, reattach disconnected directories
        #[stable(30)] #[when(ONLINE, FSCK)]
        #[depends(check_unreachable_inodes)]
        check_directory_structure,
        /// Two-pass nlink verification: collect hardlinked inodes, count
        /// directory references, fix bi_nlink mismatches
        #[stable(31)] #[when(FSCK)]
        #[depends(check_inodes, check_dirents)]
        check_nlinks,
        /// Validate reconcile work/hipri/pending/scan btrees against actual
        /// extent data; remove stale entries
        #[stable(43)] #[when(ONLINE, FSCK)]
        #[depends(check_snapshots)]
        check_reconcile_work,
        /// Resume incomplete logged operations only userspace starts (truncate,
        /// finsert, option propagation) from logged_ops btree, then delete
        /// completed entries
        #[stable(23)] #[when(ALWAYS)]
        resume_logged_ops,
        /// Scan deleted_inodes btree and fully remove inodes with nlink == 0
        /// that are not open
        #[stable(32)] #[when(ALWAYS)]
        delete_dead_inodes,
        /// Remove KEY_TYPE_inode_generation keys; this older generation
        /// persistence mechanism has been superseded
        #[stable(47)] #[when(ONLINE)]
        kill_i_generation_keys,
        /// One-time migration: clear stale front_pad/back_pad fields in
        /// KEY_TYPE_reflink_p keys
        #[stable(33)]
        fix_reflink_p,
        /// One-time pass: insert full-filesystem reconcile_scan entry to
        /// trigger background data verification
        #[stable(34)]
        set_fs_needs_reconcile,
        /// Recompute per-device btree_allocated_bitmap by scanning all live
        /// btree node pointers
        #[stable(46)] #[when(ONLINE)]
        btree_bitmap_gc,
        /// Verify root inode is readable before completing recovery
        #[stable(42)] #[when(ALWAYS, SILENT)]
        lookup_root_inode,
    }
}

/* We normally enumerate recovery passes in the order we run them: */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct recovery_pass_entry {
    pub last_run: Le64,
    pub last_runtime: Le32,
    pub flags: Le32,
}

c_bitmask! {
    LE32_BITMASK(struct recovery_pass_entry, flags) {
        BCH_RECOVERY_PASS_NO_RATELIMIT(0, 1),
    }
}

#[unsized_vstruct]
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bch_sb_field_recovery_passes {
    pub field: Head<c::bch_sb_field>,
    pub start: [c::recovery_pass_entry; 0],
}
vstruct!(unsized bch_sb_field_recovery_passes, field.u64s, start = 0);
