// SPDX-License-Identifier: GPL-2.0

//! The data types of sb/counters_format.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use zerocopy::byteorder::little_endian as le;
use cstruct_macros::{c_enum, c_xmacro, CStruct};
use typeinfo_macros::TypeInfo;

c_enum! {
    #[flags]
    pub enum bch_counters_flags: u32 {
        TYPE_COUNTER = 1 << 0,  /* event counters */
        TYPE_SECTORS = 1 << 1,  /* amount counters, the unit is sectors */
    }
}

c_xmacro! {
    BCH_PERSISTENT_COUNTERS(x) {
        (sync_fs, 110, TYPE_COUNTER, "Filesystem sync operations"),
        (fsync, 111, TYPE_COUNTER, "Fsync operations"),
        (data_read, 0, TYPE_SECTORS, "Sectors read from disk"),
        (data_read_inline, 80, TYPE_SECTORS, "Sectors read from inline data extents"),
        (data_read_hole, 81, TYPE_SECTORS, "Sectors read as holes (zero-filled)"),
        (data_read_promote, 30, TYPE_SECTORS, "Sectors promoted to cache on read"),
        (data_read_nopromote, 85, TYPE_COUNTER, "Reads not promoted"),
        (data_read_nopromote_may_not, 86, TYPE_COUNTER, "Reads not promoted: not eligible"),
        (data_read_nopromote_already_promoted, 87, TYPE_COUNTER, "Reads not promoted: already cached"),
        (data_read_nopromote_unwritten, 88, TYPE_COUNTER, "Reads not promoted: unwritten extent"),
        (data_read_nopromote_congested, 89, TYPE_COUNTER, "Reads not promoted: device congested"),
        (data_read_bounce, 31, TYPE_COUNTER, "Reads requiring bounce buffer"),
        (data_read_split, 33, TYPE_COUNTER, "Reads split across multiple extents"),
        (data_read_reuse_race, 34, TYPE_COUNTER, "Read bio reuse races"),
        (data_read_retry, 32, TYPE_COUNTER, "Read retries"),
        (data_read_fail_and_poison, 95, TYPE_COUNTER, "Read failures with poisoned pages"),
        (data_read_narrow_crcs, 97, TYPE_COUNTER, "CRC entries narrowed on read"),
        (data_read_narrow_crcs_fail, 98, TYPE_COUNTER, "CRC narrowing failures on read"),
        (data_write, 1, TYPE_SECTORS, "Sectors written to disk"),
        (data_update_pred, 96, TYPE_SECTORS, "Sectors predicted for data update"),
        (data_update, 2, TYPE_SECTORS, "Sectors moved by data update " "(reconcile, copygc)"),
        (data_update_no_io, 91, TYPE_SECTORS, "Sectors updated without IO (key update only)"),
        (data_update_in_flight, 90, TYPE_COUNTER, "Data updates currently in flight"),
        (data_update_fail, 82, TYPE_SECTORS, "Failed data update sectors"),
        (data_update_read, 35, TYPE_SECTORS, "Sectors read for data update"),
        (data_update_write, 36, TYPE_SECTORS, "Sectors written for data update"),
        (data_update_key, 37, TYPE_SECTORS, "Sectors where btree key was updated"),
        (data_update_key_fail, 38, TYPE_SECTORS, "Failed btree key update sectors"),
        (data_update_useless_write_fail, 128, TYPE_SECTORS, "Useless data update write failures"),
        (data_update_start_fail_obsolete, 39, TYPE_COUNTER, "Obsolete: data update start failures"),
        (data_update_noop_obsolete, 92, TYPE_COUNTER, "Obsolete: no-op data updates"),
        (reconcile_scan_fs, 113, TYPE_SECTORS, "Sectors scanned for filesystem reconcile"),
        (reconcile_scan_metadata, 114, TYPE_SECTORS, "Sectors scanned for metadata reconcile"),
        (reconcile_scan_pending, 115, TYPE_SECTORS, "Sectors scanned for pending reconcile"),
        (reconcile_scan_stripes, 133, TYPE_SECTORS, "Sectors scanned for stripes reconcile"),
        (reconcile_scan_device, 116, TYPE_SECTORS, "Sectors scanned for device reconcile"),
        (reconcile_scan_inum, 117, TYPE_SECTORS, "Sectors scanned for inode reconcile"),
        (reconcile_clear_scan, 129, TYPE_COUNTER, "Reconcile scan entries cleared"),
        (reconcile_btree, 118, TYPE_SECTORS, "Btree sectors reconciled"),
        (reconcile_data, 119, TYPE_SECTORS, "Data sectors reconciled"),
        (reconcile_phys, 120, TYPE_SECTORS, "Physical sectors reconciled"),
        (reconcile_stripe, 130, TYPE_SECTORS, "Stripe sectors reconciled"),
        (reconcile_set_pending, 83, TYPE_SECTORS, "Sectors marked as pending reconcile"),
        (evacuate_bucket, 84, TYPE_COUNTER, "Buckets evacuated by copygc"),
        (stripe_alloc, 125, TYPE_COUNTER, "Stripe allocation attempts"),
        (stripe_create, 102, TYPE_COUNTER, "Stripes created"),
        (stripe_reuse, 123, TYPE_COUNTER, "Stripes reused"),
        (stripe_create_fail, 103, TYPE_COUNTER, "Stripe creation failures"),
        (stripe_delete, 124, TYPE_COUNTER, "Stripes deleted"),
        (stripe_update_bucket, 104, TYPE_COUNTER, "Stripe bucket updates"),
        (stripe_update_extent, 99, TYPE_COUNTER, "Stripe extent updates"),
        (stripe_update_extent_fail, 100, TYPE_COUNTER, "Stripe extent update failures"),
        (stripe_repair_race, 131, TYPE_COUNTER, "Stripe extent update failures"),
        (copygc, 40, TYPE_COUNTER, "Copygc runs"),
        (copygc_wait_obsolete, 41, TYPE_COUNTER, "Obsolete: copygc waits"),
        (cached_ptr_drop, 121, TYPE_SECTORS, "Cached pointer sectors dropped"),
        (bucket_invalidate, 3, TYPE_COUNTER, "Buckets invalidated"),
        (bucket_discard_worker, 108, TYPE_COUNTER, "Discard worker invocations"),
        (bucket_discard_fast_worker, 109, TYPE_COUNTER, "Fast discard worker invocations"),
        (bucket_discard, 4, TYPE_COUNTER, "Bucket discards issued"),
        (bucket_discard_fast, 79, TYPE_COUNTER, "Fast bucket discards issued"),
        (bucket_alloc, 5, TYPE_COUNTER, "Bucket allocations"),
        (bucket_alloc_fail, 6, TYPE_COUNTER, "Bucket allocation failures"),
        (open_bucket_alloc_fail, 122, TYPE_COUNTER, "Open bucket allocation failures"),
        (bucket_alloc_from_stripe, 127, TYPE_COUNTER, "Buckets allocated from existing stripe"),
        (sectors_alloc, 126, TYPE_SECTORS, "Total sectors allocated"),
        (disk_reservation_degraded, 136, TYPE_SECTORS, "Sectors reserved at fewer replicas than asked for"),
        (bucket_alloc_placement_restricted, 137, TYPE_COUNTER, "Buckets allocated while free space mismatch restricted placement"),
        (bkey_pack_pos_fail, 112, TYPE_COUNTER, "Bkey position packing failures"),
        (btree_cache_scan, 7, TYPE_COUNTER, "Btree cache scan operations"),
        (btree_cache_reap, 8, TYPE_COUNTER, "Btree nodes reaped from cache"),
        (btree_cache_cannibalize, 9, TYPE_COUNTER, "Btree cache cannibalize operations"),
        (btree_cache_cannibalize_lock, 10, TYPE_COUNTER, "Btree cache cannibalize lock acquisitions"),
        (btree_cache_cannibalize_lock_fail, 11, TYPE_COUNTER, "Btree cache cannibalize lock failures"),
        (btree_cache_cannibalize_unlock, 12, TYPE_COUNTER, "Btree cache cannibalize lock releases"),
        (btree_node_write, 13, TYPE_COUNTER, "Btree node writes"),
        (btree_node_read, 14, TYPE_COUNTER, "Btree node reads"),
        (btree_node_compact, 15, TYPE_COUNTER, "Btree node compactions"),
        (btree_node_merge, 16, TYPE_COUNTER, "Btree node merges"),
        (btree_node_merge_attempt, 101, TYPE_COUNTER, "Btree node merge attempts"),
        (btree_node_split, 17, TYPE_COUNTER, "Btree node splits"),
        (btree_node_rewrite, 18, TYPE_COUNTER, "Btree node rewrites"),
        (btree_node_alloc, 19, TYPE_COUNTER, "Btree nodes allocated"),
        (btree_node_free, 20, TYPE_COUNTER, "Btree nodes freed"),
        (btree_node_set_root, 21, TYPE_COUNTER, "Btree root changes"),
        (btree_key_cache_fill, 107, TYPE_COUNTER, "Btree key cache fills"),
        (btree_path_relock_fail, 22, TYPE_COUNTER, "Btree path relock failures"),
        (btree_path_upgrade_fail, 23, TYPE_COUNTER, "Btree path lock upgrade failures"),
        (btree_reserve_get_fail, 24, TYPE_COUNTER, "Btree reservation failures"),
        (journal_flush, 135, TYPE_COUNTER, "Journal flush requested"),
        (journal_res_get_blocked, 25, TYPE_COUNTER, "Journal reservation blocked"),
        (journal_full, 26, TYPE_COUNTER, "Journal full events"),
        (journal_reclaim_finish, 27, TYPE_COUNTER, "Journal reclaim completions"),
        (journal_reclaim_start, 28, TYPE_COUNTER, "Journal reclaim starts"),
        (journal_write, 29, TYPE_COUNTER, "Journal writes"),
        (journal_pin_resize, 134, TYPE_COUNTER, "increase size of journal pin fifo"),
        (gc_gens_end, 42, TYPE_COUNTER, "GC generation pass completions"),
        (gc_gens_start, 43, TYPE_COUNTER, "GC generation pass starts"),
        (trans_blocked_journal_reclaim, 44, TYPE_COUNTER, "Transactions blocked on journal reclaim"),
        (trans_restart_btree_node_reused, 45, TYPE_COUNTER, "Transaction restart: btree node reused"),
        (trans_restart_btree_node_split, 46, TYPE_COUNTER, "Transaction restart: btree node split"),
        (trans_restart_fault_inject, 47, TYPE_COUNTER, "Transaction restart: fault injection"),
        (trans_restart_iter_upgrade, 48, TYPE_COUNTER, "Transaction restart: iterator lock upgrade"),
        (trans_restart_journal_preres_get, 49, TYPE_COUNTER, "Transaction restart: journal pre-reservation"),
        (trans_restart_journal_reclaim, 50, TYPE_COUNTER, "Transaction restart: journal reclaim"),
        (trans_restart_journal_res_get, 51, TYPE_COUNTER, "Transaction restart: journal reservation"),
        (trans_restart_key_cache_key_realloced,52, TYPE_COUNTER, "Transaction restart: key cache key reallocated"),
        (trans_restart_key_cache_raced, 53, TYPE_COUNTER, "Transaction restart: key cache race"),
        (trans_restart_mark_replicas, 54, TYPE_COUNTER, "Transaction restart: mark replicas"),
        (trans_restart_mem_realloced, 55, TYPE_COUNTER, "Transaction restart: memory reallocated"),
        (trans_restart_memory_allocation_failure, 56, TYPE_COUNTER, "Transaction restart: memory allocation failure"),
        (trans_restart_relock, 57, TYPE_COUNTER, "Transaction restart: relock"),
        (trans_restart_relock_after_fill, 58, TYPE_COUNTER, "Transaction restart: relock after fill"),
        (trans_restart_relock_key_cache_fill_obsolete, 59, TYPE_COUNTER, "Obsolete: transaction restart relock key cache fill"),
        (trans_restart_relock_next_node, 60, TYPE_COUNTER, "Transaction restart: relock next node"),
        (trans_restart_relock_parent_for_fill_obsolete, 61, TYPE_COUNTER, "Obsolete: transaction restart relock parent for fill"),
        (trans_restart_relock_path, 62, TYPE_COUNTER, "Transaction restart: relock path"),
        (trans_restart_relock_path_intent, 63, TYPE_COUNTER, "Transaction restart: relock path intent"),
        (trans_restart_too_many_iters, 64, TYPE_COUNTER, "Transaction restart: too many iterators"),
        (trans_restart_traverse, 65, TYPE_COUNTER, "Transaction restart: traverse"),
        (trans_restart_upgrade, 66, TYPE_COUNTER, "Transaction restart: lock upgrade"),
        (trans_restart_would_deadlock, 67, TYPE_COUNTER, "Transaction restart: would deadlock"),
        (trans_restart_would_deadlock_write, 68, TYPE_COUNTER, "Transaction restart: would deadlock on write"),
        (trans_restart_injected, 69, TYPE_COUNTER, "Transaction restart: injected for testing"),
        (trans_restart_key_cache_upgrade, 70, TYPE_COUNTER, "Transaction restart: key cache lock upgrade"),
        (transaction_begin, 71, TYPE_COUNTER, "Transaction start"),
        (transaction_commit, 72, TYPE_COUNTER, "Transaction commits"),
        (write_super, 73, TYPE_COUNTER, "Superblock writes"),
        (trans_restart_would_deadlock_recursion_limit, 74, TYPE_COUNTER, "Transaction restart: deadlock recursion limit"),
        (trans_restart_deadlock_waitlist_alloc, 132, TYPE_COUNTER, "Transaction restart: deadlock detector waitlist alloc fail"),
        (trans_restart_write_buffer_flush, 75, TYPE_COUNTER, "Transaction restart: write buffer flush"),
        (trans_restart_split_race, 76, TYPE_COUNTER, "Transaction restart: split race"),
        (write_buffer_flush, 105, TYPE_COUNTER, "Write buffer flushes"),
        (write_buffer_flush_slowpath, 77, TYPE_COUNTER, "Write buffer flushes via slow path"),
        (write_buffer_flush_sync, 78, TYPE_COUNTER, "Synchronous write buffer flushes"),
        (write_buffer_maybe_flush, 106, TYPE_COUNTER, "Write buffer conditional flush checks"),
        (accounting_key_to_wb_slowpath, 94, TYPE_COUNTER, "Accounting key to write buffer slow path"),
        (error_throw, 93, TYPE_COUNTER, "Errors thrown"),
    }
}

macro_rules! __bch_persistent_counters_0 {
    ([$($acc:tt)*] $(($t:tt$(, $($__rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[closed]
            pub enum bch_persistent_counters: u32 {
                $($acc)*
                $([<BCH_COUNTER_ $t>],)*
                BCH_COUNTER_NR,
            }
        }
    } };
}
BCH_PERSISTENT_COUNTERS!(__bch_persistent_counters_0 []);

macro_rules! __bch_persistent_counters_stable_0 {
    ([$($acc:tt)*] $(($t:tt, $n:expr$(, $($__rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_persistent_counters_stable: u32 {
                $($acc)*
                $([<BCH_COUNTER_STABLE_ $t>] = (($n) as u32),)*
                BCH_COUNTER_STABLE_NR,
            }
        }
    } };
}
BCH_PERSISTENT_COUNTERS!(__bch_persistent_counters_stable_0 []);

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_sb_field_counters {
    pub field: c::bch_sb_field,
    pub d: [le::U64; 0],
}
