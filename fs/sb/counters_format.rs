// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::Le64;
use crate::util::vstructs::{vstruct, Head};
use cstruct_macros::{bch_enum, unsized_vstruct, MaybeZeroable, Plain, TypeInfo};

#[bch_enum(flags)]
#[repr(u32)]
pub enum CounterFlags {
    TYPE_COUNTER = 1 << 0,  /* event counters */
    TYPE_SECTORS = 1 << 1,  /* amount counters, the unit is sectors */
}

#[bch_enum(closed)]
#[prefix(BCH_COUNTER_)]
#[nr]
#[arm_columns(stable: enum(PersistentCounterStable: u32, prefix = BCH_COUNTER_STABLE_, nr),
              flags: CounterFlags)]
#[repr(u32)]
pub enum PersistentCounter {
    /// Filesystem sync operations
    #[stable(110)] #[flags(TYPE_COUNTER)] sync_fs,
    /// Fsync operations
    #[stable(111)] #[flags(TYPE_COUNTER)] fsync,
    /// Sectors read from disk
    #[stable(0)] #[flags(TYPE_SECTORS)] data_read,
    /// Sectors read from inline data extents
    #[stable(80)] #[flags(TYPE_SECTORS)] data_read_inline,
    /// Sectors read as holes (zero-filled)
    #[stable(81)] #[flags(TYPE_SECTORS)] data_read_hole,
    /// Sectors promoted to cache on read
    #[stable(30)] #[flags(TYPE_SECTORS)] data_read_promote,
    /// Reads not promoted
    #[stable(85)] #[flags(TYPE_COUNTER)] data_read_nopromote,
    /// Reads not promoted: not eligible
    #[stable(86)] #[flags(TYPE_COUNTER)] data_read_nopromote_may_not,
    /// Reads not promoted: already cached
    #[stable(87)] #[flags(TYPE_COUNTER)] data_read_nopromote_already_promoted,
    /// Reads not promoted: unwritten extent
    #[stable(88)] #[flags(TYPE_COUNTER)] data_read_nopromote_unwritten,
    /// Reads not promoted: device congested
    #[stable(89)] #[flags(TYPE_COUNTER)] data_read_nopromote_congested,
    /// Reads requiring bounce buffer
    #[stable(31)] #[flags(TYPE_COUNTER)] data_read_bounce,
    /// Reads split across multiple extents
    #[stable(33)] #[flags(TYPE_COUNTER)] data_read_split,
    /// Read bio reuse races
    #[stable(34)] #[flags(TYPE_COUNTER)] data_read_reuse_race,
    /// Read retries
    #[stable(32)] #[flags(TYPE_COUNTER)] data_read_retry,
    /// Read failures with poisoned pages
    #[stable(95)] #[flags(TYPE_COUNTER)] data_read_fail_and_poison,
    /// CRC entries narrowed on read
    #[stable(97)] #[flags(TYPE_COUNTER)] data_read_narrow_crcs,
    /// CRC narrowing failures on read
    #[stable(98)] #[flags(TYPE_COUNTER)] data_read_narrow_crcs_fail,
    /// Sectors written to disk
    #[stable(1)] #[flags(TYPE_SECTORS)] data_write,
    /// Sectors predicted for data update
    #[stable(96)] #[flags(TYPE_SECTORS)] data_update_pred,
    /// Sectors moved by data update (reconcile, copygc)
    #[stable(2)] #[flags(TYPE_SECTORS)] data_update,
    /// Sectors updated without IO (key update only)
    #[stable(91)] #[flags(TYPE_SECTORS)] data_update_no_io,
    /// Data updates currently in flight
    #[stable(90)] #[flags(TYPE_COUNTER)] data_update_in_flight,
    /// Failed data update sectors
    #[stable(82)] #[flags(TYPE_SECTORS)] data_update_fail,
    /// Sectors read for data update
    #[stable(35)] #[flags(TYPE_SECTORS)] data_update_read,
    /// Sectors written for data update
    #[stable(36)] #[flags(TYPE_SECTORS)] data_update_write,
    /// Sectors where btree key was updated
    #[stable(37)] #[flags(TYPE_SECTORS)] data_update_key,
    /// Failed btree key update sectors
    #[stable(38)] #[flags(TYPE_SECTORS)] data_update_key_fail,
    /// Useless data update write failures
    #[stable(128)] #[flags(TYPE_SECTORS)] data_update_useless_write_fail,
    /// Obsolete: data update start failures
    #[stable(39)] #[flags(TYPE_COUNTER)] data_update_start_fail_obsolete,
    /// Obsolete: no-op data updates
    #[stable(92)] #[flags(TYPE_COUNTER)] data_update_noop_obsolete,
    /// Sectors scanned for filesystem reconcile
    #[stable(113)] #[flags(TYPE_SECTORS)] reconcile_scan_fs,
    /// Sectors scanned for metadata reconcile
    #[stable(114)] #[flags(TYPE_SECTORS)] reconcile_scan_metadata,
    /// Sectors scanned for pending reconcile
    #[stable(115)] #[flags(TYPE_SECTORS)] reconcile_scan_pending,
    /// Sectors scanned for stripes reconcile
    #[stable(133)] #[flags(TYPE_SECTORS)] reconcile_scan_stripes,
    /// Sectors scanned for device reconcile
    #[stable(116)] #[flags(TYPE_SECTORS)] reconcile_scan_device,
    /// Sectors scanned for inode reconcile
    #[stable(117)] #[flags(TYPE_SECTORS)] reconcile_scan_inum,
    /// Reconcile scan entries cleared
    #[stable(129)] #[flags(TYPE_COUNTER)] reconcile_clear_scan,
    /// Btree sectors reconciled
    #[stable(118)] #[flags(TYPE_SECTORS)] reconcile_btree,
    /// Data sectors reconciled
    #[stable(119)] #[flags(TYPE_SECTORS)] reconcile_data,
    /// Physical sectors reconciled
    #[stable(120)] #[flags(TYPE_SECTORS)] reconcile_phys,
    /// Stripe sectors reconciled
    #[stable(130)] #[flags(TYPE_SECTORS)] reconcile_stripe,
    /// Sectors marked as pending reconcile
    #[stable(83)] #[flags(TYPE_SECTORS)] reconcile_set_pending,
    /// Reconcile thread exits on an error
    #[stable(140)] #[flags(TYPE_COUNTER)] reconcile_error,
    /// Buckets evacuated by copygc
    #[stable(84)] #[flags(TYPE_COUNTER)] evacuate_bucket,
    /// Stripe allocation attempts
    #[stable(125)] #[flags(TYPE_COUNTER)] stripe_alloc,
    /// Stripes created
    #[stable(102)] #[flags(TYPE_COUNTER)] stripe_create,
    /// Stripes reused
    #[stable(123)] #[flags(TYPE_COUNTER)] stripe_reuse,
    /// Stripe creation failures
    #[stable(103)] #[flags(TYPE_COUNTER)] stripe_create_fail,
    /// Stripes deleted
    #[stable(124)] #[flags(TYPE_COUNTER)] stripe_delete,
    /// Stripe bucket updates
    #[stable(104)] #[flags(TYPE_COUNTER)] stripe_update_bucket,
    /// Stripe extent updates
    #[stable(99)] #[flags(TYPE_COUNTER)] stripe_update_extent,
    /// Stripe extent update failures
    #[stable(100)] #[flags(TYPE_COUNTER)] stripe_update_extent_fail,
    /// Stripe repairs that found nothing to repair
    #[stable(131)] #[flags(TYPE_COUNTER)] stripe_repair_race,
    /// Stripe repairs evacuating blocks, to narrow the stripe
    #[stable(141)] #[flags(TYPE_COUNTER)] stripe_repair_narrow,
    /// Copygc runs
    #[stable(40)] #[flags(TYPE_COUNTER)] copygc,
    /// Obsolete: copygc waits
    #[stable(41)] #[flags(TYPE_COUNTER)] copygc_wait_obsolete,
    /// Cached pointer sectors dropped
    #[stable(121)] #[flags(TYPE_SECTORS)] cached_ptr_drop,
    /// Buckets invalidated
    #[stable(3)] #[flags(TYPE_COUNTER)] bucket_invalidate,
    /// Discard worker invocations
    #[stable(108)] #[flags(TYPE_COUNTER)] bucket_discard_worker,
    /// Fast discard worker invocations
    #[stable(109)] #[flags(TYPE_COUNTER)] bucket_discard_fast_worker,
    /// Bucket discards issued
    #[stable(4)] #[flags(TYPE_COUNTER)] bucket_discard,
    /// Fast bucket discards issued
    #[stable(79)] #[flags(TYPE_COUNTER)] bucket_discard_fast,
    /// Bucket allocations
    #[stable(5)] #[flags(TYPE_COUNTER)] bucket_alloc,
    /// Bucket allocation failures
    #[stable(6)] #[flags(TYPE_COUNTER)] bucket_alloc_fail,
    /// Open bucket allocation failures
    #[stable(122)] #[flags(TYPE_COUNTER)] open_bucket_alloc_fail,
    /// Buckets allocated from existing stripe
    #[stable(127)] #[flags(TYPE_COUNTER)] bucket_alloc_from_stripe,
    /// Total sectors allocated
    #[stable(126)] #[flags(TYPE_SECTORS)] sectors_alloc,
    /// Sectors reserved at fewer replicas than asked for
    #[stable(136)] #[flags(TYPE_SECTORS)] disk_reservation_degraded,
    /// Disk reservations that failed with ENOSPC
    #[stable(145)] #[flags(TYPE_COUNTER)] disk_reservation_enospc,
    /// Buckets allocated while free space mismatch restricted placement
    #[stable(137)] #[flags(TYPE_COUNTER)] bucket_alloc_placement_restricted,
    /// Allocations waiting past allocator_stuck_timeout
    #[stable(138)] #[flags(TYPE_COUNTER)] allocator_stuck,
    /// Bkey position packing failures
    #[stable(112)] #[flags(TYPE_COUNTER)] bkey_pack_pos_fail,
    /// Btree cache scan operations
    #[stable(7)] #[flags(TYPE_COUNTER)] btree_cache_scan,
    /// Btree nodes reaped from cache
    #[stable(8)] #[flags(TYPE_COUNTER)] btree_cache_reap,
    /// Btree cache cannibalize operations
    #[stable(9)] #[flags(TYPE_COUNTER)] btree_cache_cannibalize,
    /// Btree cache cannibalize lock acquisitions
    #[stable(10)] #[flags(TYPE_COUNTER)] btree_cache_cannibalize_lock,
    /// Btree cache cannibalize lock failures
    #[stable(11)] #[flags(TYPE_COUNTER)] btree_cache_cannibalize_lock_fail,
    /// Btree cache cannibalize lock releases
    #[stable(12)] #[flags(TYPE_COUNTER)] btree_cache_cannibalize_unlock,
    /// Btree node writes
    #[stable(13)] #[flags(TYPE_COUNTER)] btree_node_write,
    /// Btree node reads
    #[stable(14)] #[flags(TYPE_COUNTER)] btree_node_read,
    /// Btree node compactions
    #[stable(15)] #[flags(TYPE_COUNTER)] btree_node_compact,
    /// Btree node merges
    #[stable(16)] #[flags(TYPE_COUNTER)] btree_node_merge,
    /// Btree node merge attempts
    #[stable(101)] #[flags(TYPE_COUNTER)] btree_node_merge_attempt,
    /// Btree node splits
    #[stable(17)] #[flags(TYPE_COUNTER)] btree_node_split,
    /// Btree node rewrites
    #[stable(18)] #[flags(TYPE_COUNTER)] btree_node_rewrite,
    /// Btree nodes allocated
    #[stable(19)] #[flags(TYPE_COUNTER)] btree_node_alloc,
    /// Btree nodes freed
    #[stable(20)] #[flags(TYPE_COUNTER)] btree_node_free,
    /// Btree root changes
    #[stable(21)] #[flags(TYPE_COUNTER)] btree_node_set_root,
    /// Btree key cache fills
    #[stable(107)] #[flags(TYPE_COUNTER)] btree_key_cache_fill,
    /// Btree path relock failures
    #[stable(22)] #[flags(TYPE_COUNTER)] btree_path_relock_fail,
    /// Btree path lock upgrade failures
    #[stable(23)] #[flags(TYPE_COUNTER)] btree_path_upgrade_fail,
    /// Btree reservation failures
    #[stable(24)] #[flags(TYPE_COUNTER)] btree_reserve_get_fail,
    /// Journal flush requested
    #[stable(135)] #[flags(TYPE_COUNTER)] journal_flush,
    /// Journal reservation blocked
    #[stable(25)] #[flags(TYPE_COUNTER)] journal_res_get_blocked,
    /// Journal full events
    #[stable(26)] #[flags(TYPE_COUNTER)] journal_full,
    /// Journal reclaim completions
    #[stable(27)] #[flags(TYPE_COUNTER)] journal_reclaim_finish,
    /// Journal reclaim starts
    #[stable(28)] #[flags(TYPE_COUNTER)] journal_reclaim_start,
    /// Journal writes
    #[stable(29)] #[flags(TYPE_COUNTER)] journal_write,
    /// increase size of journal pin fifo
    #[stable(134)] #[flags(TYPE_COUNTER)] journal_pin_resize,
    /// Journal scrubs run, at recovery
    #[stable(139)] #[flags(TYPE_COUNTER)] journal_scrub,
    /// Journal flush ranges the journal scrub checked
    #[stable(144)] #[flags(TYPE_COUNTER)] journal_scrub_range,
    /// Journal scrubs finding btree nodes with no good replica
    #[stable(142)] #[flags(TYPE_COUNTER)] journal_scrub_lost_btree_nodes,
    /// Extents and btree nodes the journal scrub repaired from a good
    /// replica
    #[stable(143)] #[flags(TYPE_COUNTER)] journal_scrub_repaired,
    /// GC generation pass completions
    #[stable(42)] #[flags(TYPE_COUNTER)] gc_gens_end,
    /// GC generation pass starts
    #[stable(43)] #[flags(TYPE_COUNTER)] gc_gens_start,
    /// Transactions blocked on journal reclaim
    #[stable(44)] #[flags(TYPE_COUNTER)] trans_blocked_journal_reclaim,
    /// Transaction restart: btree node reused
    #[stable(45)] #[flags(TYPE_COUNTER)] trans_restart_btree_node_reused,
    /// Transaction restart: btree node split
    #[stable(46)] #[flags(TYPE_COUNTER)] trans_restart_btree_node_split,
    /// Transaction restart: fault injection
    #[stable(47)] #[flags(TYPE_COUNTER)] trans_restart_fault_inject,
    /// Transaction restart: iterator lock upgrade
    #[stable(48)] #[flags(TYPE_COUNTER)] trans_restart_iter_upgrade,
    /// Transaction restart: journal pre-reservation
    #[stable(49)] #[flags(TYPE_COUNTER)] trans_restart_journal_preres_get,
    /// Transaction restart: journal reclaim
    #[stable(50)] #[flags(TYPE_COUNTER)] trans_restart_journal_reclaim,
    /// Transaction restart: journal reservation
    #[stable(51)] #[flags(TYPE_COUNTER)] trans_restart_journal_res_get,
    /// Transaction restart: key cache key reallocated
    #[stable(52)] #[flags(TYPE_COUNTER)] trans_restart_key_cache_key_realloced,
    /// Transaction restart: key cache race
    #[stable(53)] #[flags(TYPE_COUNTER)] trans_restart_key_cache_raced,
    /// Transaction restart: mark replicas
    #[stable(54)] #[flags(TYPE_COUNTER)] trans_restart_mark_replicas,
    /// Transaction restart: memory reallocated
    #[stable(55)] #[flags(TYPE_COUNTER)] trans_restart_mem_realloced,
    /// Transaction restart: memory allocation failure
    #[stable(56)] #[flags(TYPE_COUNTER)] trans_restart_memory_allocation_failure,
    /// Transaction restart: relock
    #[stable(57)] #[flags(TYPE_COUNTER)] trans_restart_relock,
    /// Transaction restart: relock after fill
    #[stable(58)] #[flags(TYPE_COUNTER)] trans_restart_relock_after_fill,
    /// Obsolete: transaction restart relock key cache fill
    #[stable(59)] #[flags(TYPE_COUNTER)] trans_restart_relock_key_cache_fill_obsolete,
    /// Transaction restart: relock next node
    #[stable(60)] #[flags(TYPE_COUNTER)] trans_restart_relock_next_node,
    /// Obsolete: transaction restart relock parent for fill
    #[stable(61)] #[flags(TYPE_COUNTER)] trans_restart_relock_parent_for_fill_obsolete,
    /// Transaction restart: relock path
    #[stable(62)] #[flags(TYPE_COUNTER)] trans_restart_relock_path,
    /// Transaction restart: relock path intent
    #[stable(63)] #[flags(TYPE_COUNTER)] trans_restart_relock_path_intent,
    /// Transaction restart: too many iterators
    #[stable(64)] #[flags(TYPE_COUNTER)] trans_restart_too_many_iters,
    /// Transaction restart: traverse
    #[stable(65)] #[flags(TYPE_COUNTER)] trans_restart_traverse,
    /// Transaction restart: lock upgrade
    #[stable(66)] #[flags(TYPE_COUNTER)] trans_restart_upgrade,
    /// Transaction restart: would deadlock
    #[stable(67)] #[flags(TYPE_COUNTER)] trans_restart_would_deadlock,
    /// Transaction restart: would deadlock on write
    #[stable(68)] #[flags(TYPE_COUNTER)] trans_restart_would_deadlock_write,
    /// Transaction restart: injected for testing
    #[stable(69)] #[flags(TYPE_COUNTER)] trans_restart_injected,
    /// Transaction restart: key cache lock upgrade
    #[stable(70)] #[flags(TYPE_COUNTER)] trans_restart_key_cache_upgrade,
    /// Transaction start
    #[stable(71)] #[flags(TYPE_COUNTER)] transaction_begin,
    /// Transaction commits
    #[stable(72)] #[flags(TYPE_COUNTER)] transaction_commit,
    /// Superblock writes
    #[stable(73)] #[flags(TYPE_COUNTER)] write_super,
    /// Transaction restart: deadlock recursion limit
    #[stable(74)] #[flags(TYPE_COUNTER)] trans_restart_would_deadlock_recursion_limit,
    /// Transaction restart: deadlock detector waitlist alloc fail
    #[stable(132)] #[flags(TYPE_COUNTER)] trans_restart_deadlock_waitlist_alloc,
    /// Transaction restart: write buffer flush
    #[stable(75)] #[flags(TYPE_COUNTER)] trans_restart_write_buffer_flush,
    /// Transaction restart: split race
    #[stable(76)] #[flags(TYPE_COUNTER)] trans_restart_split_race,
    /// Write buffer flushes
    #[stable(105)] #[flags(TYPE_COUNTER)] write_buffer_flush,
    /// Write buffer flushes via slow path
    #[stable(77)] #[flags(TYPE_COUNTER)] write_buffer_flush_slowpath,
    /// Synchronous write buffer flushes
    #[stable(78)] #[flags(TYPE_COUNTER)] write_buffer_flush_sync,
    /// Write buffer conditional flush checks
    #[stable(106)] #[flags(TYPE_COUNTER)] write_buffer_maybe_flush,
    /// Accounting key to write buffer slow path
    #[stable(94)] #[flags(TYPE_COUNTER)] accounting_key_to_wb_slowpath,
    /// Errors thrown
    #[stable(93)] #[flags(TYPE_COUNTER)] error_throw,
}

#[unsized_vstruct]
#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo)]
pub struct bch_sb_field_counters {
    pub field: Head<c::bch_sb_field>,
    pub d: [Le64; 0],
}
vstruct!(unsized bch_sb_field_counters, field.u64s, start = 0);
