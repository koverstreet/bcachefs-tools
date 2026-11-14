// SPDX-License-Identifier: GPL-2.0

//! The data types, a module per file - foo/bar.rs is foo_bar - and c, all
//! of them in one namespace. btree/types.rs, btree/node/, fs/dirent_format.rs
//! and format.rs's are modules of their own already.

#[path = "alloc/disk_groups_format.rs"] pub mod alloc_disk_groups_format;
#[path = "alloc/format.rs"] pub mod alloc_format;
#[path = "alloc/lru_format.rs"] pub mod alloc_lru_format;
#[path = "alloc/replicas_format.rs"] pub mod alloc_replicas_format;
#[path = "alloc/types.rs"] pub mod alloc_types;
#[path = "bcachefs_ioctl.rs"] pub mod bcachefs_ioctl;
#[path = "btree/bkey_defs.rs"] pub mod btree_bkey_defs;
#[path = "btree/bkey_types.rs"] pub mod btree_bkey_types;
#[path = "btree/journal_overlay_defs.rs"] pub mod btree_journal_overlay_defs;
#[path = "btree/write_buffer_defs.rs"] pub mod btree_write_buffer_defs;
#[path = "btree/check_types.rs"] pub mod btree_check_types;
#[path = "btree/key_cache_types.rs"] pub mod btree_key_cache_types;
#[path = "data/ec/create_types.rs"] pub mod data_ec_create_types;
#[path = "data/ec/format.rs"] pub mod data_ec_format;
#[path = "data/ec/types.rs"] pub mod data_ec_types;
#[path = "data/extents_format.rs"] pub mod data_extents_format;
#[path = "data/extents_sb_format.rs"] pub mod data_extents_sb_format;
#[path = "data/move_types.rs"] pub mod data_move_types;
#[path = "data/reconcile/format.rs"] pub mod data_reconcile_format;
#[path = "data/reconcile/types.rs"] pub mod data_reconcile_types;
#[path = "data/reflink_format.rs"] pub mod data_reflink_format;
#[path = "data/write_types.rs"] pub mod data_write_types;
#[path = "debug/trace_types.rs"] pub mod debug_trace_types;
#[path = "fs/acl_format.rs"] pub mod fs_acl_format;
#[path = "fs/inode_format.rs"] pub mod fs_inode_format;
#[path = "fs/logged_ops_format.rs"] pub mod fs_logged_ops_format;
#[path = "fs/quota_format.rs"] pub mod fs_quota_format;
#[path = "fs/xattr_format.rs"] pub mod fs_xattr_format;
#[path = "init/damage_format.rs"] pub mod init_damage_format;
#[path = "init/passes_format.rs"] pub mod init_passes_format;
#[path = "journal/journal_types.rs"] pub mod journal_journal_types;
#[path = "journal/seq_blacklist_format.rs"] pub mod journal_seq_blacklist_format;
#[path = "journal/types.rs"] pub mod journal_types;
#[path = "sb/counters_format.rs"] pub mod sb_counters_format;
#[path = "sb/downgrade_format.rs"] pub mod sb_downgrade_format;
#[path = "sb/errors_format.rs"] pub mod sb_errors_format;
#[path = "snapshots/format.rs"] pub mod snapshots_format;
#[path = "snapshots/types.rs"] pub mod snapshots_types;
#[path = "types.rs"] pub mod types;
#[path = "util/two_state_shared_lock_types.rs"] pub mod util_two_state_shared_lock_types;
#[path = "util/util_types.rs"] pub mod util_util_types;
#[path = "vfs/buffered_types.rs"] pub mod vfs_buffered_types;
#[path = "vfs/direct_types.rs"] pub mod vfs_direct_types;
#[path = "vfs/fdm_types.rs"] pub mod vfs_fdm_types;
#[path = "vfs/fs_types.rs"] pub mod vfs_fs_types;
#[path = "vfs/io_types.rs"] pub mod vfs_io_types;
#[path = "vfs/pagecache_types.rs"] pub mod vfs_pagecache_types;
#[path = "vfs/types.rs"] pub mod vfs_types;

/// What C's types are to Rust: every type and constant above, and the
/// kernel's - C's one namespace. The converted types name each other through
/// it, as C does.
pub mod c {
    #![allow(unused_imports, ambiguous_glob_reexports)]

    #[cfg(not(kernel))]
    pub use bcachefs_shim::c::*;
    #[cfg(kernel)]
    pub use kernel::bindings::*;

    #[cfg(not(kernel))]
    pub use crate::c::{cds_list_head, cds_hlist_head, cds_hlist_node, list_head, hlist_head, hlist_node, timespec64};
    #[cfg(kernel)]
    pub use crate::c::rcu_head;

    // vendor/'s C, not converted.
    pub use crate::c::{closure, closure_waitlist};

    // C's locks, as the structs C shares hold them: written through shared
    // references - locking/raw.rs.
    pub use crate::locking::{mutex, percpu_rw_semaphore, raw_spinlock_t, rw_semaphore, spinlock_t};

    // C's limits, which the constants use.
    pub const U8_MAX:  u8  = u8::MAX;
    pub const U16_MAX: u16 = u16::MAX;
    pub const U32_MAX: u32 = u32::MAX;
    pub const U64_MAX: u64 = u64::MAX;
    pub const INT_MAX: i32 = i32::MAX;

    pub use crate::btree::types::*;
    pub use crate::btree::path::{BtreePath, btree_path_bits, PathIdx, btree_path_level};
    pub(crate) use crate::btree::path::__btree_path_bits_forward;
    pub use crate::dirent::format::*;
    pub use crate::format::accounting::*;
    pub use crate::alloc::buckets::{ ReservationFlags, disk_reservation_destructable};
    pub use crate::alloc::buckets::{BCH_DEV_POS_FRAC_BITS, BucketData, BucketGensArray, BucketGensHead, DiskReservation, RESERVE_FACTOR, bch_dev_usage, bch_dev_usage_full, bch_dev_usage_type, bch_fs_placeable, bch_fs_usage_base, bch_fs_usage_short, bucket};
    pub use crate::alloc::disk_groups::{ TARGET_DEV_START, TARGET_GROUP_START, TargetType, target_id};
    pub use super::alloc_disk_groups_format::*;
    pub use crate::alloc::disk_groups::{DiskGroups, bch_disk_group_cpu};
    pub use crate::alloc::foreground::{AllocBtreeBitmap, alloc_request, alloc_request_bits, alloc_trace_entry, alloc_trace_entry_bits, dev_alloc_list};
    pub use super::alloc_format::*;
    pub use super::alloc_lru_format::*;
    pub use crate::alloc::replicas::{ WriteCheck};
    pub use super::alloc_replicas_format::*;
    pub use crate::alloc::replicas::{bch_replicas_padded};
    pub use super::alloc_types::*;
    pub use crate::format::*;
    pub use super::bcachefs_ioctl::*;
    pub use crate::btree::bbpos::{bbpos, blbpos, blbpos_bits};
    pub(crate) use crate::btree::bbpos::{__blbpos_bits_forward};
    pub use crate::btree::bkey_buf::{bkey_buf};
    pub use super::btree_bkey_defs::*;
    pub use super::btree_bkey_types::*;
    pub use crate::btree::node::bset::{BSET_TREE_NR_TYPES, MAX_BSETS};
    pub use super::btree_check_types::*;
    pub use crate::btree::interior::{BTREE_NODE_RESERVE, BTREE_RESERVE_MAX, BtreeReserveCache, bch_fs_btree_interior_updates, bch_fs_btree_reserve_cache, btree_alloc};
    pub use crate::btree::iter::{trans_for_each_path_inorder_iter, trans_may_drop_updates};
    pub use super::btree_journal_overlay_defs::*;
    pub use crate::btree::journal_overlay::{OverwriteRange, OverwriteRanges, OverwriteRangesLive, OverwriteRangesLiveWriter, journal_key, journal_key_flags, journal_key_id, journal_key_journal_pos, journal_key_src, journal_keys, journal_ptr, journal_replay};
    pub(crate) use crate::btree::journal_overlay::{__journal_key_flags_forward, __journal_key_id_forward};
    pub use super::btree_key_cache_types::*;
    pub use crate::btree::locking::{ BtreeNodeLockedType};
    pub use crate::btree::update::{ TransCommitFlagBit, TransCommitFlags};
    pub use super::btree_write_buffer_defs::*;
    pub use crate::btree::write_buffer::{ BTREE_WRITE_BUFERED_VAL_U64s_MAX, bch_fs_btree_write_buffer, WbBtree, btree_write_buffered_key, wb_flushing_pin, wb_inc_pin, WbFlushCaller, wb_key_ref, wb_key_ref_key, wb_key_ref_pos_idx, wb_key_ref_words};
    pub use crate::data::compress::{ bbuf, BbufType, bch_compression_opt, bch_compression_opt_bits, bch_compression_opt_fields};
    pub(crate) use crate::data::compress::{__bch_compression_opt_bits_forward};
    pub use crate::data::copygc::{bch_fs_copygc};
    pub use super::data_ec_create_types::*;
    pub use super::data_ec_format::*;
    pub use super::data_ec_types::*;
    pub use crate::data::extents::{bch_extent_crc, bkey_durability, bkey_ptrs, bkey_ptrs_c};
    pub use super::data_extents_format::*;
    pub use super::data_extents_sb_format::*;
    pub use crate::data::extents::{ ReadFlagBit, bch_io_failures, bch_extent_crc_unpacked, ReadFlags, extent_ptr_decoded};
    pub use super::data_move_types::*;
    pub use crate::data::nocow_locking::{BUCKET_NOCOW_LOCKS, BUCKET_NOCOW_LOCKS_BITS, NOCOW_LOCK_BUCKET_SIZE};
    pub use super::data_reconcile_format::*;
    pub use crate::data::reconcile::trigger::{ per_snapshot_io_opts, SetNeedsReconcileCtx, snapshot_io_opts_entry};
    pub use super::data_reconcile_types::*;
    pub use crate::data::reconcile::work::ReconcileScan;
    pub use super::data_reflink_format::*;
    pub use crate::data::update::{ DataUpdateType, data_update, data_update_key, data_update_opts, data_update_opts_bits};
    pub(crate) use crate::data::update::{__data_update_opts_bits_forward};
    pub use super::data_write_types::*;
    pub use crate::debug::async_objs::{ async_obj_list, AsyncObjKind};
    pub(crate) use super::debug_trace_types::*;
    pub use crate::errcode::{ Errcode, bch_errcode_max, blk_status_to_bch_err};
    pub use crate::errcode::status::BLK_STS_REMOVED;
    pub(crate) use crate::errcode::{BCH_ERRCODES, BLK_ERRS, ZSTD_ERRS, __BCH_ERRCODES_0, __BCH_ERRCODES_1};
    pub use super::fs_acl_format::*;
    pub use crate::dirent::{ RenameMode};
    pub use super::fs_inode_format::*;
    pub use crate::inode_opts::{inode_opt_change};
    pub use crate::inode::{VolatileInodeUnpacked, bch_inode_unpacked, bkey_inode_buf, u96};
    pub use super::fs_logged_ops_format::*;
    pub use crate::logged_ops::{bch_fs_logged_ops};
    pub use super::fs_quota_format::*;
    pub use crate::quota::{Memquota, MemquotaCounter, MemquotaType, QuotaAcctMode, QuotaLimit, Quotas, bch_qid};
    pub use crate::str_hash::{bch_hash_info};
    pub use super::fs_xattr_format::*;
    pub use super::init_damage_format::*;
    pub use crate::init::dev::{AtomicDevsMask, DevsMask, bch_devs_list, bch_devs_mask, bch_fs_sb, bch_sb_bdev, bch_sb_buf, bch_sb_handle, bch_sb_handle_bits, bch_sb_io};
    pub(crate) use crate::init::dev::{__bch_sb_handle_bits_forward};
    pub use crate::init::error::{bch_fs_errors, fsck_damaged_path};
    pub(crate) use crate::init::error::{ErrorsLocked, FsckErrState};
    pub use crate::init::passes::{ RunRecoveryPassFlags};
    pub use super::init_passes_format::*;
    pub use crate::init::passes::{RecoveryWords, RecoveryWordsWriter, bch_fs_recovery, bch_recovery_runner};
    pub use crate::init::progress::{progress_indicator};
    pub use super::journal_journal_types::*;
    pub use super::journal_seq_blacklist_format::*;
    pub use super::journal_types::*;
    pub use crate::opts::{AtomicOptsMask, BCACHEFS_VERBOSE_DEFAULT, BCH_OPTS_MAX, OPT_SHOW_FULL_LIST, OPT_SHOW_MOUNT_STYLE, PublishedOpts, PublishedOptsWriter, RATELIMIT_ERRORS_DEFAULT, bch_inode_opts, OptId, bch_opts, bch_opts_mask, FsckErrOpt, OptFlags};
    pub use super::sb_counters_format::*;
    pub use crate::sb::counters::{NR_RECENT_COUNTERS, bch_fs_counters};
    pub use super::sb_downgrade_format::*;
    pub use super::sb_errors_format::*;
    pub use crate::sb::errors::{bch_sb_error_entry_cpu, bch_sb_error_entry_cpu_bits};
    pub(crate) use crate::sb::errors::{__bch_sb_error_entry_cpu_bits_forward};
    pub use crate::sb::io::{ SbWriteFlags};
    pub use crate::sb::io::{bch_sb_cpu, bch_sb_cpuWriter};
    pub use crate::sb::members::{bch_dev_identity};
    pub use crate::format::members::*;
    pub use crate::sb::members::{bch_member_cpu};
    pub use super::snapshots_format::*;
    pub use super::snapshots_format::Snapshot;
    pub use super::snapshots_types::*;
    pub use super::types::*;
    pub use crate::util::clock::{io_clock, IO_CLOCK_PCPU_SECTORS};
    pub use crate::data_structures::fast_list::{fast_list, fast_list_pcpu};
    pub use crate::locking::{mutex_noio, percpu_rwsem_noio};
    pub use crate::util::mean_and_variance::{mean_and_variance};
    pub use crate::util::printbuf::{PRINTBUF_INLINE_TABSTOPS, printbuf_bits, printbuf_restore, PrintbufSi};
    pub(crate) use crate::util::printbuf::{__printbuf_bits_forward};
    pub use crate::locking::seqmutex::seqmutex;
    pub use crate::util::siphash::{SIPHASH_KEY};
    pub use crate::locking::six::*;
    pub use crate::util::time_stats::{NR_QUANTILES, TIME_STATS_MV_WEIGHT, TimeStatsLocked, bch2_time_stats, bch2_time_stats_quantiles, quantile_entry, quantiles, time_stat_buffer, time_stat_buffer_entry};
    pub use super::util_two_state_shared_lock_types::*;
    pub use super::util_util_types::*;
    #[cfg(not(NO_BCACHEFS_FS))]
    pub use super::vfs_buffered_types::*;
    #[cfg(not(NO_BCACHEFS_FS))]
    pub use super::vfs_direct_types::*;
    pub use super::vfs_fdm_types::*;
    pub use super::vfs_fs_types::*;
    #[cfg(not(NO_BCACHEFS_FS))]
    pub use super::vfs_io_types::*;
    #[cfg(not(NO_BCACHEFS_FS))]
    pub use super::vfs_pagecache_types::*;
    pub use super::vfs_types::*;
    pub use crate::alloc::background::{BUCKET_GC_GEN_MAX};
    pub use crate::btree::cache::bch2_btree_id_level_to_text;
    pub use crate::namei::{BCH_CREATE_SNAPSHOT, BCH_CREATE_SNAPSHOT_RO, BCH_CREATE_SUBVOL, BCH_CREATE_TMPFILE};
}
