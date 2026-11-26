// SPDX-License-Identifier: GPL-2.0

//! c: the bindings, and the Rust types still named through it - C's one
//! namespace, which their users leave for the types' own modules.

pub mod c {
    #![allow(unused_imports, ambiguous_glob_reexports)]

    #[cfg(not(kernel))]
    pub use bcachefs_shim::c::*;
    #[cfg(not(kernel))]
    pub use crate::os::c::*;
    #[cfg(kernel)]
    pub use kernel::bindings::*;

    #[cfg(kernel)]
    pub use crate::c::rcu_head;

    // C's limits, which the constants use.
    pub const U8_MAX:  u8  = u8::MAX;
    pub const U16_MAX: u16 = u16::MAX;
    pub const U32_MAX: u32 = u32::MAX;
    pub const U64_MAX: u64 = u64::MAX;
    pub const INT_MAX: i32 = i32::MAX;

    pub use crate::btree::{BtreeNodeType, bch_fs_btree, btree_max_u64s};
    pub use crate::btree::locking::btree_bkey_cached_common;
    pub use crate::btree::cache::{BTREE_EVICTED_SIZE_HASH_BITS, BTREE_EVICTED_SIZE_HASH_MASK, BtreeCacheGuard, BtreeCacheLists,
                                  BtreeCacheLive, BtreeCacheLiveNr, BtreeCacheLiveNrWriter, BtreeCacheLiveWriter,
                                  BtreeCacheLockAt, BtreeCacheNotFreedReason, BtreeNodeCacheState, BtreeNodeList,
                                  bch_fs_btree_cache, btree_cache_list, btree_cache_shrinker, btree_evicted_size};
    pub(crate) use crate::btree::cache::BtreeCacheMutex;
    pub use crate::btree::interior::BtreeNodeRewriteReason;
    pub(crate) use crate::btree::interior::BtreeNodeRewriteReason_arms;
    pub use crate::btree::interior::{BTREE_ROOT_LEVEL_BITS, BtreeNodeSibling, BtreeRoots, BtreeRootsGuard, BtreeRootsLive,
                                     BtreeRootsLiveWriter, btree_root};
    pub use crate::btree::iter::{BtreeIter, BtreeIterFlagBit, BtreeIterUpdateTriggerFlags, btree_iter_bits};
    pub use crate::btree::key_cache::{BKEY_CACHED_ACCESSED, BKEY_CACHED_DIRTY, BKEY_CACHED_IMMEDIATE_FLUSH, bkey_cached};
    pub use crate::btree::update::{btree_insert_entry, btree_insert_entry_bits, btree_trans_commit_hook};
    pub use crate::btree::path::{BTREE_ITER_INITIAL, BTREE_ITER_MAX, btree_trans_paths};
    pub use crate::btree::trans::{BCH_TRANSACTIONS_NR, BTREE_TRANS_MAX_LOCK_HOLD_TIME_NS, BTREE_TRANS_MEM_MAX, BtreeTrans, TransMem,
                                  TransStatsJson, TransStatsLocked, TransStatsMax, TransStatsMaxWriter, bch_fs_btree_trans,
                                  btree_trans_bits, btree_trans_buf, btree_transaction_stats, trans_kmalloc_trace};
    pub use crate::btree::node::{BTREE_WRITE_TYPE_BITS, BtreeNodeFlags, BtreeWriteType};
    pub use crate::btree::write::btree_write_stats;
    pub use crate::btree::path::{BtreePath, btree_path_bits, PathIdx, btree_path_level};
    pub(crate) use crate::btree::path::__btree_path_bits_forward;
    pub use crate::posix::dirent_format::*;
    pub use crate::alloc::accounting_format::*;
    pub use crate::alloc::buckets::ReservationFlags;
    pub use crate::alloc::buckets::{BCH_DEV_POS_FRAC_BITS, BucketData, BucketGensArray, BucketGensHead, DiskReservation, RESERVE_FACTOR, bch_dev_usage, bch_dev_usage_full, bch_dev_usage_type, bch_fs_placeable, bch_fs_usage_base, bch_fs_usage_short, bucket};
    pub use crate::alloc::disk_groups::{ TARGET_DEV_START, TARGET_GROUP_START};
    pub use crate::alloc::disk_groups_format::*;
    pub use crate::alloc::foreground::{AllocBtreeBitmap, alloc_request, alloc_request_bits, alloc_trace_entry, alloc_trace_entry_bits, dev_alloc_list};
    pub use crate::alloc::format::*;
    pub use crate::alloc::lru_format::*;
    pub use crate::alloc::replicas::{ WriteCheck};
    pub use crate::alloc::replicas_format::*;
    pub use crate::alloc::replicas::{bch_replicas_padded};
    pub use crate::alloc::foreground::{BCH_WATERMARK_BITS, BCH_WATERMARK_MASK, OPEN_BUCKETS_COUNT, OpenBucketId, OpenBucketIdentity,
                                     OpenBucketIdentityWriter, OpenBucketIdx, OpenBucketsFree, OpenBucketsFreeNr,
                                     OpenBucketsFreeNrWriter, Watermark, bch_fs_allocator, open_bucket, open_bucket_idx_t,
                                     open_buckets};
    pub use crate::alloc::write_point::{WRITE_POINT_HASH_NR, WRITE_POINT_MAX, WritePointId, WritePointIdWriter, WritePointIdx,
                                        WritePointLocked, WritePointsHash, WritePointsHashWriter, write_point,
                                        write_point_alloc, write_point_specifier};
    pub use crate::alloc::dev_select::{Placement, PlacementWriter, dev_stripe_state};
    pub use crate::alloc::capacity::{MarkLockAt, bch_fs_capacity, bch_fs_capacity_counters, bch_fs_capacity_pcpu};
    pub use crate::alloc::discard::bch_fs_discards;
    pub use crate::format::*;
    pub use crate::bcachefs_ioctl::*;
    pub use crate::btree::bbpos::{bbpos, blbpos, blbpos_bits};
    pub(crate) use crate::btree::bbpos::{__blbpos_bits_forward};
    pub use crate::btree::bkey::{BkeyPackPosRet, BkeyValidateFrom, ValidateFlags, bkey_format_state, bkey_packed_padded,
                                 bkey_validate_context, bkey_validate_context_bits, bkey_validate_context_root_bits};
    pub use crate::btree::node::bset::{BSET_TREE_NR_TYPES, MAX_BSETS};
    pub use crate::btree::check_allocations::{GcPhase, bch_fs_gc, gc_btree_order, gc_pos, gc_pos_bits};
    pub use crate::alloc::gc_gens::bch_fs_gc_gens;
    pub use crate::btree::interior::{BTREE_NODE_RESERVE, BTREE_RESERVE_MAX, BtreeReserveCache, bch_fs_btree_interior_updates, bch_fs_btree_reserve_cache, btree_alloc};
    pub use crate::btree::iter::trans_may_drop_updates;
    pub use crate::btree::journal_overlay::{OverwriteRange, OverwriteRanges, OverwriteRangesLive, OverwriteRangesLiveWriter, journal_iter, journal_key, journal_key_flags, journal_key_id, journal_key_journal_pos, journal_key_src, journal_keys, journal_ptr, journal_replay};
    pub(crate) use crate::btree::journal_overlay::{__journal_key_flags_forward, __journal_key_id_forward};
    pub use crate::btree::key_cache::{bch_fs_btree_key_cache, bkey_cached_key, btree_key_cache_shrinker};
    pub use crate::btree::locking::{ BtreeNodeLockedType};
    pub use crate::btree::update::{ TransCommitFlagBit, TransCommitFlags, btree_trans_subbuf};
    pub use crate::btree::write_buffer::{ BTREE_WRITE_BUFERED_VAL_U64s_MAX, bch_fs_btree_write_buffer, WbBtree, btree_write_buffered_key, WbFlushCaller, wb_key_ref};
    pub use crate::data::compress::bch_compression_opt;
    pub(crate) use crate::data::compress::{__bch_compression_opt_forward};
    pub use crate::data::copygc::{bch_fs_copygc};
    pub use crate::data::ec::head::{DevStripes, EcStripeNewState, HeadDevs, HeadState, StripeBlockMask,
                                    ec_dev_stripe_state, ec_stripe_head};
    pub use crate::data::ec::format::*;
    pub use crate::data::ec::bch_fs_ec;
    pub use crate::data::ec::trigger::{GcStripeData, gc_stripe, gc_stripe_bits};
    pub use crate::data::extents::bkey_durability;
    pub use crate::data::extents_format::*;
    pub use crate::data::extents_sb_format::*;
    pub use crate::data::extents::{ ReadFlagBit, bch_io_failures, bch_extent_crc_unpacked, ReadFlags, extent_ptr_decoded};
    pub use crate::data::moving::{MoveStats, bch_move_stats_at, move_bucket_key};
    pub use crate::data::nocow_locking::{BUCKET_NOCOW_LOCKS, BUCKET_NOCOW_LOCKS_BITS, NOCOW_LOCK_BUCKET_SIZE};
    pub use crate::data::reconcile::format::*;
    pub use crate::data::reconcile::trigger::{ per_snapshot_io_opts, SetNeedsReconcileCtx, snapshot_io_opts_entry};
    pub use crate::data::reconcile::bch_fs_reconcile;
    pub use crate::data::reconcile::work::ReconcileScan;
    pub use crate::data::reflink_format::*;
    pub use crate::data::update::{ DataUpdateType, data_update, data_update_index, data_update_key, data_update_opts,
                                   data_update_opts_bits};
    pub(crate) use crate::data::update::{__data_update_opts_bits_forward};
    pub use crate::data::write::{WriteFlagBit, WriteFlags, bch_write_op, bch_write_op_bits};
    pub use crate::debug::async_objs::{ async_obj_list, AsyncObjKind};
    pub use crate::errcode::{ Errcode, blk_status_to_bch_err};
    pub use crate::errcode::status::BLK_STS_REMOVED;
    pub(crate) use crate::errcode::{BCH_ERRCODES, BLK_ERRS, ZSTD_ERRS, __BCH_ERRCODES_0, __BCH_ERRCODES_1};
    pub use crate::posix::acl_format::*;
    pub use crate::posix::dirent::{ RenameMode};
    pub use crate::posix::inode_format::*;
    pub use crate::posix::inode_opts::{inode_opt_change};
    pub use crate::posix::inode::{VolatileInodeUnpacked, bch_inode_unpacked, u96};
    pub use crate::posix::logged_ops_format::*;
    pub use crate::posix::logged_ops::{bch_fs_logged_ops};
    pub use crate::posix::quota_format::*;
    pub use crate::posix::quota::{Memquota, MemquotaCounter, MemquotaType, QuotaAcctMode, QuotaLimit, Quotas, bch_qid};
    pub use crate::posix::str_hash::{bch_hash_info};
    pub use crate::posix::xattr_format::*;
    pub use crate::init::damage_format::*;
    pub use crate::init::dev::{AtomicDevsMask, DevsMask, bch_devs_list, bch_devs_mask, bch_fs_sb, bch_sb_bdev, bch_sb_buf, bch_sb_handle, bch_sb_io};
    pub use crate::init::error::{bch_fs_errors, fsck_damaged_path};
    pub(crate) use crate::init::error::{ErrorsLocked, FsckErrState};
    pub use crate::init::passes::{ RunRecoveryPassFlags};
    pub use crate::init::passes_format::*;
    pub use crate::init::passes::{RecoveryWords, RecoveryWordsWriter, bch_fs_recovery, bch_recovery_runner};
    pub use crate::init::progress::{progress_indicator};
    pub use crate::journal::journal::{JOURNAL_RES_GET_CHECK, JOURNAL_RES_GET_NONBLOCK, JournalCycleFlags, JournalResFlag};
    pub use crate::journal::seq_blacklist_format::*;
    pub use crate::journal::journal::{JOURNAL_ENTRY_BLOCKED_VAL, JOURNAL_ENTRY_CLOSED_VAL, JOURNAL_ENTRY_ERROR_VAL,
                                      JOURNAL_ENTRY_OFFSET_MAX, JOURNAL_ENTRY_SIZE_MAX, JOURNAL_ENTRY_SIZE_MIN,
                                      JOURNAL_SEQ_MAX, JOURNAL_STATE_BUF0_SHIFT, JOURNAL_STATE_BUF_BITS,
                                      JOURNAL_STATE_BUF_COUNT_BITS, JOURNAL_STATE_BUF_COUNT_MAX, JOURNAL_STATE_BUF_MASK,
                                      JOURNAL_STATE_BUF_NR, JournalFlag, JournalGuard, JournalHot, JournalHotWriter,
                                      JournalLockAt, JournalLocked, JournalPublished, JournalPublishedWriter,
                                      ReclaimThread, RewindRanges, journal, journal_entry_res, journal_res,
                                      journal_res_state, journal_res_state_bits, journal_rewind_range};
    pub use crate::journal::journal::{InFlight, InFlightLocked, JOURNAL_BUF_NR, journal_buf};
    pub use crate::journal::write::{BufJset, BufLockGuard, BufLocked, JournalKey, JournalWriteRef, JsetBuf, WriteAlloc,
                                    journal_buf_bits, jset_buf_alloc};
    pub use crate::journal::pins::{JournalPinType, PinBuf, PinFlushFn, PinLocked, PinLockedBy, PinRange, journal_entry_pin};
    pub use crate::journal::reclaim::{JournalSpaceFrom, ReclaimPublished, ReclaimPublishedWriter, journal_space};
    pub use crate::journal::buckets::{JournalBucketStage, journal_device};
    pub use crate::journal::init::journal_start_info;
    pub use crate::opts::{AtomicOptsMask, BCACHEFS_VERBOSE_DEFAULT, BCH_OPTS_MAX, OPT_SHOW_FULL_LIST, OPT_SHOW_MOUNT_STYLE, PublishedOpts, PublishedOptsWriter, RATELIMIT_ERRORS_DEFAULT, bch_inode_opts, OptId, bch_opts, bch_opts_mask, FsckErrOpt, OptFlags};
    pub use crate::sb::counters_format::*;
    pub use crate::sb::counters::{NR_RECENT_COUNTERS, bch_fs_counters};
    pub use crate::sb::downgrade_format::*;
    pub use crate::sb::errors_format::*;
    pub use crate::sb::errors::{bch_sb_error_entry_cpu, bch_sb_error_entry_cpu_bits};
    pub(crate) use crate::sb::errors::{__bch_sb_error_entry_cpu_bits_forward};
    pub use crate::sb::io::{ SbWriteFlags};
    pub use crate::sb::io::{bch_sb_cpu, bch_sb_cpuWriter};
    pub use crate::sb::members::{bch_dev_identity};
    pub use crate::sb::members_format::*;
    pub use crate::sb::members::{bch_member_cpu};
    pub use crate::snapshots::format::*;
    pub use crate::snapshots::format::Snapshot;
    pub use crate::snapshots::bch_fs_snapshots;
    pub use crate::snapshots::snapshot::{IS_ANCESTOR_BITMAP, SnapshotIdState, SnapshotTable, snapshot_t};
    pub use crate::snapshots::subvolume::subvol_inum;
    pub use crate::fs::{Fs, FsFlag};
    pub use crate::init::dev::{CONGESTED_MAX, Dev, DevFlag, io_count};
    pub use crate::sb::members::{DevReadRefKind, DevWriteRefKind};
    pub use crate::alloc::check_backpointers::{BucketBitmapWords, BucketBits, BucketBitsWriter, bucket_bitmap};
    pub use crate::btree::interior::GC_MERGE_NODES;
    #[cfg(CONFIG_DEBUG_FS)]
    pub use crate::debug::debug::btree_debug;
    pub use crate::util::time_stats::TimeStat;
    pub(crate) use crate::util::time_stats::TimeStat_arms;
    pub use crate::util::clock::{io_clock, IO_CLOCK_PCPU_SECTORS};
    pub use crate::data_structures::fast_list::fast_list;
    pub use crate::util::mean_and_variance::{mean_and_variance};
    pub use crate::locking::seqmutex::seqmutex;
    pub use crate::util::siphash::{SIPHASH_KEY};
    pub use crate::locking::six::*;
    pub use crate::util::time_stats::{NR_QUANTILES, TIME_STATS_MV_WEIGHT, TimeStatsLocked, bch2_time_stats, bch2_time_stats_quantiles, quantile_entry, quantiles, time_stat_buffer, time_stat_buffer_entry};
    pub use crate::locking::{two_state_lock_t, TwoState, TwoStateGuard};
    #[cfg(kernel)]
    pub use crate::vfs::fdm::{FDM_HASH_BITS, FDM_HASH_SIZE, FDM_NR_HASH, fdm_hash, fdm_slot};
    #[cfg(kernel)]
    pub use crate::vfs::inode::{InodeInfo, InodeInfoFlag, InodeQuota, InodeReserved, InodeReservedRange,
                                InodeReservedRangeWriter, PagecacheAdd, PagecacheBlock, quota_res};
    #[cfg(kernel)]
    pub use crate::vfs::pagecache::{BCH_WRITEPAGE_BUF_BYTES, FolioSectorState, FolioState, bch_folio,
                                    bch_folio_sector, bch_folio_sector_bits};
    #[cfg(kernel)]
    pub use crate::vfs::bch_fs_vfs;
    pub use crate::alloc::background::{BUCKET_GC_GEN_MAX};
    pub use crate::btree::cache::bch2_btree_id_level_to_text;
    pub use crate::posix::namei::{BCH_CREATE_SNAPSHOT, BCH_CREATE_SNAPSHOT_RO, BCH_CREATE_SUBVOL, BCH_CREATE_TMPFILE};
}
