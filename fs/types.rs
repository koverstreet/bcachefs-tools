// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::locking::{Lock, LockedBy, MutexNoio, PercpuRwsemNoio};
use crate::util::vstructs::Head;
use crate::types::{Atomic64, GenRadix, RustOnly, bits_to_longs, c_default, rust_only};
use crate::util::ffi::c_char;
use core::ffi::{c_int, c_uint, c_ulong, c_ushort};
use cstruct_macros::{c_const, c_enum, c_rcu_array, c_xmacro, CStruct, Init, TypeInfo};
use pin_init::pin_data;

c_xmacro! {
    /* Parameters that are useful for debugging, but should always be compiled in: */
    BCH_DEBUG_PARAMS_ALWAYS(BCH_DEBUG_PARAM) {
        (key_merging_disabled,
         "Disables merging of extents"),
        (btree_node_merging_disabled,
         "Disables merging of btree nodes"),
        (btree_gc_always_rewrite,
         "Causes mark and sweep to compact and rewrite every "
         "btree node it traverses"),
        (btree_gc_rewrite_disabled,
         "Disables rewriting of btree nodes during mark and sweep"),
        (btree_shrinker_disabled,
         "Disables the shrinker callback for the btree node cache"),
        (backpointers_no_use_write_buffer,
         "Don't use the write buffer for backpointers, enabling "
         "extra runtime checks"),
        (debug_check_btree_locking,
         "Enable additional asserts for btree locking"),
        (debug_check_iterators,
         "Enables extra verification for btree iterators"),
        (debug_check_bset_lookups,
         "Enables extra verification for bset lookups"),
        (debug_check_btree_accounting,
         "Verify btree accounting for keys within a node"),
        (debug_check_bkey_unpack,
         "Enables extra verification for bkey unpack"),
    }
}

c_xmacro! {
    /* Parameters that should only be compiled in debug mode: */
    BCH_DEBUG_PARAMS_DEBUG(BCH_DEBUG_PARAM) {
        (journal_seq_verify,
         "Store the journal sequence number in the version "
         "number of every btree key, and verify that btree "
         "update ordering is preserved during recovery"),
        (inject_invalid_keys,
         "Store the journal sequence number in the version "
         "number of every btree key, and verify that btree "
         "update ordering is preserved during recovery"),
        (test_alloc_startup,
         "Force allocator startup to use the slowpath where it"
         "can't find enough free buckets without invalidating"
         "cached data"),
        (force_reconstruct_read,
         "Force reads to use the reconstruct path, when reading"
         "from erasure coded extents"),
        (test_restart_gc,
         "Test restarting mark and sweep gc when bucket gens change"),
    }
}

c_xmacro! {
    BCH_TIME_STATS(x) {
        (btree_node_mem_alloc,
         "Allocate memory in the btree node cache "
         "for a new btree node"),
        (btree_node_split,
         "Split a full btree node into two new nodes"),
        (btree_node_compact,
         "Compact a full btree node on disk"),
        (btree_node_merge,
         "Merge two adjacent btree nodes"),
        (btree_node_sort,
         "Sort and resort entire btree nodes in memory, "
         "after reading from disk or for compacting"),
        (btree_node_read,
         "Read btree nodes from disk"),
        (btree_node_read_done,
         "Post-read btree node processing"),
        (btree_node_write,
         "Write btree node to disk"),
        (btree_interior_update_foreground,
         "Foreground time for topology-changing btree updates "
         "(splits, compactions, merges); roughly corresponds "
         "to lock held time"),
        (btree_interior_update_total,
         "Total time for topology-changing btree updates, "
         "including background transaction phase after "
         "new nodes are written"),
        (btree_node_cache_scan,
         "scan btree node cache for eviction"),
        (btree_key_cache_scan,
         "scan btree key cache for eviction"),
        (btree_write_buffer_flush,
         "Flush btree write buffer to btree"),
        (btree_write_buffer_flush_shard_sched_delay,
         "Per-shard: queued to start of execution (workqueue scheduling delay)"),
        (btree_write_buffer_flush_shard_work,
         "Per-shard: actual flush work duration (wb_flush_sorted_range)"),
        (btree_gc,
         "GC pass recalculating oldest generation numbers"),
        (data_write,
         "Core write path: allocate space, compress, "
         "encrypt, checksum, issue writes, "
         "update extents btree"),
        (data_read,
         "Core read path: look up extents btree, "
         "issue reads, checksum, decrypt, decompress"),
        (data_promote,
         "Promote: write a cached copy of an extent "
         "to promote_target on read"),
        (journal_flush_write,
         "Flush journal writes: cache flush to devices "
         "then FUA journal writes"),
        (journal_noflush_write,
         "Non-flush journal writes, without cache "
         "flushes or FUA"),
        (journal_flush_seq,
         "Flush a journal sequence number to disk "
         "for sync, fsync, and bucket reuse"),
        (journal_pin_flush_btree,
         "Flush btree journal pins"),
        (journal_pin_flush_key_cache,
         "Flush key cache journal pins"),
        (journal_pin_flush_other,
         "Flush other journal pins"),
        (sb_write,
         "Write the superblock to every member device; "
         "held under sb_lock, so this is what other "
         "superblock writers wait behind"),
        (blocked_journal_low_on_space,
         "Blocked: journal reclaim not keeping up "
         "with reclaiming space"),
        (blocked_journal_low_on_pin,
         "Blocked: journal pins (dirty btree nodes, "
         "key cache entries) not flushed fast enough"),
        (blocked_journal_low_on_open_buckets,
         "Blocked: open buckets running low, throttling new "
         "journal work so reclaim can free them"),
        (blocked_journal_max_in_flight,
         "Blocked: too many journal writes in flight"),
        (blocked_journal_max_open,
         "Blocked: too many journal entries open, "
         "not yet closed for writing"),
        (blocked_journal_blocked,
         "Blocked: waiting for write buffer flush"),
        (blocked_journal_full,
         "Blocked: writer hit journal_full (no room in current "
         "entry, reclaim not keeping up)"),
        (blocked_journal_pin_full,
         "Blocked: writer hit journal_pin_full (pin fifo full, "
         "btree node / key cache flushers not keeping up)"),
        (blocked_journal_buf_enomem,
         "Blocked: writer hit journal_buf_enomem (preallocated "
         "data buffer not topped up)"),
        (blocked_journal_stuck,
         "Blocked: writer hit journal_stuck (10s timeout fired "
         "in slowpath wait)"),
        (blocked_key_cache_flush,
         "Blocked: waiting for key cache flush"),
        (blocked_allocate,
         "Blocked: bucket allocation waiting, copygc or "
         "allocator thread not keeping up"),
        (blocked_allocate_open_bucket,
         "Blocked: all open bucket handles in use"),
        (blocked_write_buffer_full,
         "Blocked: write buffer full"),
        (blocked_writeback_throttle,
         "Blocked: writeback throttle"),
        (nocow_lock_contended,
         "Nocow lock contention"),
        (blocked_discard_journal_flush,
         "Blocked: discard worker waiting for journal flush "
         "to advance rewind_seq and release buckets"),
    }
}

c_enum! {
    #[closed]
    pub enum bch_time_stats: u32 {
        BCH_TIME_STATS(name, ..) => BCH_TIME_ ## name,
        BCH_TIME_STAT_NR,
    }
}

c_const! {
    /* Number of nodes btree coalesce will try to coalesce at once */
    pub const GC_MERGE_NODES: u32 = 4;

    pub const BTREE_NODE_OPEN_BUCKET_RESERVE: u32 = c::BTREE_RESERVE_MAX * c::BCH_REPLICAS_MAX;
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct io_count {
    pub sectors: [[u64; c::BCH_DATA_NR as usize]; 2],
}

c_xmacro! {
    BCH_DEV_READ_REFS(x) {
        (bch2_online_devs),
        (trans_mark_dev_sbs),
        (read_fua_test),
        (sb_field_resize),
        (write_super),
        (journal_read),
        (fs_journal_alloc),
        (fs_resize_on_mount),
        (fs_mi_field_upgrades),
        (sb_journal_sort),
        (btree_node_read),
        (btree_node_read_all_replicas),
        (btree_node_scrub),
        (btree_node_write),
        (btree_node_scan),
        (btree_verify_replicas),
        (btree_node_ondisk_to_text),
        (io_read),
        (check_extent_checksums),
        (ec_block),
    }
}

c_enum! {
    #[closed]
    pub enum bch_dev_read_ref: u32 {
        BCH_DEV_READ_REFS(n) => BCH_DEV_READ_REF_ ## n,
        BCH_DEV_READ_REF_NR,
    }
}

c_xmacro! {
    BCH_DEV_WRITE_REFS(x) {
        (journal_write),
        (journal_discard),
        (discard_bucket),
        (discard_one_bucket_fast),
        (do_invalidates),
        (stripe_update_extents),
        (nocow_flush),
        (io_write),
        (ec_block),
        (ec_bucket_zero),
    }
}

c_enum! {
    #[closed]
    pub enum bch_dev_write_ref: u32 {
        BCH_DEV_WRITE_REFS(n) => BCH_DEV_WRITE_REF_ ## n,
        BCH_DEV_WRITE_REF_NR,
    }
}

c_rcu_array! {
    /// A bucket_bitmap's words: replaced by a copy when the device grows.
    pub type BucketBitmapWords = crate::util::rcu::RcuArray<crate::util::bitmap::AtomicULong>
        as struct bucket_bitmap_words { nr, d };
}

crate::published! {
    /// A bucket_bitmap's bits: read without its lock - by copygc, while a
    /// check sets and frees them - so they're atomic words under RCU.
    pub struct BucketBits {
        pub buckets: c::BucketBitmapWords,
        /// Bits set
        pub nr:      crate::util::projections::Volatile<u64>,
    }
}

/// One bit per bucket of a device, allocated when the first is set:
/// check_extents_to_backpointers' buckets whose backpointers don't add up, and
/// those with none. The lock serializes setting, growing and freeing; C only
/// initializes it, and calls Rust for the rest.
#[repr(C)]
#[derive(CStruct)]
pub struct bucket_bitmap {
    #[c_anon("struct mutex lock; struct bucket_bitmap_words __rcu *buckets; u64 nr")]
    pub bits: Lock<c::mutex, (), BucketBits>,
}
c_default!(bucket_bitmap);

c_xmacro! {
    /*
     * unflushed_writes: a write completed that the device may still hold in its
     * volatile cache; journal_write_preflush() clears it when it flushes the device.
     */
    BCH_DEV_FLAGS(x) {
        (unflushed_writes),
    }
}

c_enum! {
    #[closed]
    pub enum bch_dev_flags: u32 {
        BCH_DEV_FLAGS(n) => BCH_DEV_ ## n,
    }
}

c_const! {
    #[c_int]
    pub const CONGESTED_MAX: u32 = 1024;
}

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_dev {
    pub kobj: c::kobject,
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    pub ref_: c::atomic_long_t,
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    pub dying: bool,
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    pub last_put: c_ulong,
    #[cfg(not(CONFIG_BCACHEFS_DEBUG))]
    pub ref_: c::percpu_ref,
    pub ref_completion: c::completion,
    /*
     * ref_outer keeps the bch_dev allocation alive - nothing more.
     * Unlike ca->ref, holding it does NOT mean the device is still a
     * member of the filesystem.
     *
     * ca->ref drains inside the device removal protocol, under
     * state_lock - so a ca->ref holder must never block on state_lock.
     * Contexts that do block on state_lock with a device in hand (work
     * items, ioctl lookups) hold ref_outer instead, and recheck
     * ca->removing under the lock before touching member state.
     */
    pub ref_outer: c::refcount_t,
    pub ref_outer_completion: c::completion,
    pub io_ref: [c::enumerated_ref; 2],

    pub fs: *mut c::bch_fs,

    pub dev_idx: u8,
    /*
     * Device is being removed and its alloc info and stripe pointers are
     * about to be deleted: new references must not be created. Checked by
     * __ec_stripe_create(), which may hold pre-invalidation copies of
     * stripe pointers; set by bch2_dev_remove() before the data drop,
     * cleared if removal fails.
     */
    pub removing: bool,
    /*
     * Cached version of this device's member info from superblock
     * Committed by bch2_write_super() -> bch_fs_mi_update()
     */
    pub mi: c::bch_member_cpu,
    pub btree_allocated_bitmap_gc: u64,
    #[c("atomic64_t errors[BCH_MEMBER_ERROR_NR]")]
    pub errors: [Atomic64; c::BCH_MEMBER_ERROR_NR as usize],
    pub write_errors_start: c_ulong,
    pub flags: c_ulong,

    pub uuid: c::__uuid_t,
    pub name: [c_char; c::BDEVNAME_SIZE as usize],

    /// Its superblock, sb_lock's - through Fs::dev_sb() - and the block
    /// device it's on, fixed while it's attached.
    #[c("struct bch_sb_handle disk_sb")]
    pub disk_sb: LockedBy<c::bch_sb_io, c::bch_sb_bdev>,
    pub sb_read_scratch: *mut Head<c::bch_sb>,
    pub sb_write_error: c_int,
    pub dev: c::dev_t,

    pub self_mask: c::bch_devs_mask,

    /*
     * Buckets:
     * Per-bucket arrays are protected by either rcu_read_lock or
     * state_lock, for device resize.
     */
    pub buckets_gc: GenRadix<c::bucket>,
    pub bucket_gens: c::BucketGensArray,
    pub oldest_gen: *mut u8,
    pub buckets_nouse: *mut c_ulong,

    pub bucket_backpointer_mismatch: c::bucket_bitmap,
    pub bucket_backpointer_empty: c::bucket_bitmap,

    #[c("struct bch_dev_usage_full __percpu *usage")]
    pub usage: *mut c::bch_dev_usage_full,

    /* Allocator: */
    pub alloc_cursor: [u64; 3],

    /*
     * Incremented by bch2_alloc_wake_dev() at every site that wakes
     * freelist_wait for a specific device. Waiters on
     * c->allocator.freelist_wait snapshot the counters for the devices
     * they need at park time and compare on wake: if none have advanced,
     * the wake didn't concern this waiter and it re-parks without the
     * full alloc retry. Fs-wide wakes bump allocator.wake_all_counter
     * instead (see bch2_alloc_wake_all()).
     */
    pub alloc_wake_counter: c::atomic_t,

    /* Buckets copygc has queued for evacuation on this device: */
    pub copygc_in_flight: c::atomic_t,

    pub nr_open_buckets: c_uint,
    pub nr_partial_buckets: c_uint,
    pub nr_btree_reserve: c_uint,

    /* this device's share of c->capacity.reserved, which is their sum */
    pub reserved_sectors: u64,

    pub invalidate_work: c::work_struct,

    pub discard_fast_work: c::work_struct,
    pub discard_fast: c::darray_u64,
    pub discard_fast_lock: c::mutex,

    pub rebalance_work: c::atomic64_t,

    pub journal: c::journal_device,
    pub prev_journal_sector: u64,

    pub io_error_work: c::work_struct,

    /* The rest of this all shows up in sysfs */
    #[c("atomic64_t cur_latency[2]")]
    pub cur_latency: [Atomic64; 2],
    pub io_latency: [c::bch2_time_stats_quantiles; 2],

    pub congested: c::atomic_t,
    pub congested_last: u64,

    #[c("struct io_count __percpu *io_done")]
    pub io_done: *mut c::io_count,
}
c_default!(bch_dev);

c_xmacro! {
    /*
     * initial_gc_unfixed
     * error
     * topology error
     */
    BCH_FS_FLAGS(x) {
        (new_fs),
        (start_begun),
        (started),
        (clean_recovery),
        (btree_running),
        (accounting_replay_done),
        (may_go_rw),
        (scrub_journal),
        (may_upgrade_downgrade),
        (rw),
        (rw_init_done),
        (was_rw),
        (stopping),
        (emergency_ro),
        (going_ro),
        (write_disable_complete),
        (clean_shutdown),
        (in_recovery),
        (running_recovery_passes),
        (in_fsck),
        (initial_gc_unfixed),
        (error),
        (topology_error),
        (errors_fixed),
        (errors_fixed_silent),
        (errors_not_fixed),
        (no_invalid_checks),
        (discard_mount_opt_set),
        (sb_dirty),
        (all_devs_rw),
    }
}

c_enum! {
    #[closed]
    pub enum bch_fs_flags: u32 {
        BCH_FS_FLAGS(n) => BCH_FS_ ## n,
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct btree_debug {
    pub id: c_uint,
}

c_const! {
    pub const BCH_LINK_MAX: u32 = c::U32_MAX;
}

c_xmacro! {
    BCH_WRITE_REFS(x) {
        (journal),
        (trans),
        (write),
        (promote),
        (node_rewrite),
        (stripe_create),
        (stripe_delete),
        (reflink),
        (fallocate),
        (fsync),
        (dio_write),
        (discard),
        (discard_fast),
        (check_discard_freespace_key),
        (invalidate),
        (gc_gens),
        (presplit_shard_boundaries),
        (snapshot_delete_pagecache),
        (sysfs),
        (btree_write_buffer),
        (btree_node_scrub),
        (async_recovery_passes),
        (ioctl_data),
    }
}

c_enum! {
    #[closed]
    pub enum bch_write_ref: u32 {
        BCH_WRITE_REFS(n) => BCH_WRITE_REF_ ## n,
        BCH_WRITE_REF_NR,
    }
}

/// The filesystem: struct bch_fs, to C. Rust allocates it and frees it -
/// bch2_fs_alloc_mem(), bch2_fs_free_mem() - so its Rust-typed fields get
/// their initializers, from Fs::init(), and their Drop; C's bch2_fs_init()
/// and teardown do the rest, as they did with kvzalloc() and kvfree().
/// Borrowed by Rust as &Fs; OpenFs owns one opened from userspace.
#[pin_data]
#[repr(C)]
#[derive(CStruct, TypeInfo, Init)]
#[c_name(bch_fs)]
#[cfg_attr(kernel, cfi_encoding = "6bch_fs")]
pub struct Fs {
    pub cl: c::closure,

    pub list: c::list_head,
    pub kobj: c::kobject,
    pub counters_kobj: c::kobject,
    pub internal: c::kobject,
    pub opts_dir: c::kobject,
    pub time_stats: c::kobject,
    pub time_stats_json: c::kobject,
    pub flags: c_ulong,

    pub minor: c_int,
    pub chardev: *mut c::device,
    pub vfs_sb: *mut c::super_block,
    pub dev: c::dev_t,
    pub name: [c_char; 40],

    pub stdio: *mut c::stdio_redirect,
    pub stdio_filter: *mut c::task_struct,
    /*
     * Online fsck redirects everything it prints; mounting doesn't - a boot
     * splash wants the questions and the hard errors, not a running
     * commentary of IO errors. Set this and the redirect carries only what
     * bch2_print_str_user() sends.
     */
    pub stdio_user_only: bool,
    /*
     * Someone is polling BCH_IOCTL_RECOVERY_STATUS on that redirect, so
     * they're drawing progress themselves and progress indicators keep off
     * the console entirely. Set by the first such ioctl, cleared when the
     * channel detaches - a mount that asks for a status fd but never polls
     * still gets its progress in dmesg.
     */
    pub stdio_progress_reader: bool,
    pub loglevel: c_uint,
    pub prev_loglevel: c_uint,
    /*
     * Certain operations are only allowed in single threaded mode, during
     * recovery, and we want to assert that this is the case:
     */
    pub recovery_task: *mut c::task_struct,

    /* ro/rw, add/remove/resize devices: */
    pub state_lock: c::rw_semaphore,

    /* Counts outstanding writes, for clean transition to read-only */
    pub writes: c::enumerated_ref,

    /*
     * Analagous to c->writes, for asynchronous ops that don't necessarily
     * need fs to be read-write
     */
    pub ro_ref: c::refcount_t,
    pub ro_ref_wait: c::wait_queue_head_t,
    pub read_only_work: c::work_struct,

    #[c("struct bch_dev __rcu *devs[BCH_SB_MEMBERS_MAX]")]
    pub devs: [crate::util::rcu::RcuPtr<c::bch_dev>; c::BCH_SB_MEMBERS_MAX as usize],
    #[c("struct bch_devs_mask devs_online")]
    pub devs_online: c::AtomicDevsMask,
    #[c("struct bch_devs_mask devs_removed")]
    pub devs_removed: c::AtomicDevsMask,
    #[c("struct bch_devs_mask devs_rotational")]
    pub devs_rotational: c::AtomicDevsMask,

    pub opts: c::bch_opts,
    pub opt_change_lock: c::mutex,
    pub opt_change_cookie: u32,
    pub mount_opts: c::bch_opts_mask,

    pub sb_write: c::closure,
    /// The superblock, c->disk_sb, behind sb_lock - Fs::sb_lock() is the way
    /// in - and what of it is read without the lock, c->sb, which it
    /// publishes.
    #[pin]
    #[init(crate::new_lock!(Default::default(), ::pin_init::zeroed(), "&c->sb_lock"))]
    #[c_anon("struct mutex_noio sb_lock; struct bch_fs_sb disk_sb; struct bch_sb_cpu sb")]
    pub sb: MutexNoio<c::bch_fs_sb, c::bch_sb_cpu>,
    #[c("unsigned long incompat_versions_requested[BITS_TO_LONGS(BCH_VERSION_MINOR(bcachefs_metadata_version_current))]")]
    pub incompat_versions_requested: crate::util::bitmap::AtomicBitArray<{
        bits_to_longs(c::BCH_VERSION_MINOR(c::bcachefs_metadata_version_current as u64) as usize) }>,
    pub cf_encoding: *mut c::unicode_map,

    pub block_bits: c_ushort, /* ilog2(block_size) */

    /*
     * shard → preferred CPU mapping for wake_cpu hinting from
     * bch2_trans_begin(): each shard's worth of btree-node working set
     * gravitates to a fixed CPU's L1/L2.
     */
    pub inode_shard_cpu: [u16; 256],

    #[c("struct delayed_work maybe_schedule_btree_bitmap_gc")]
    #[pin]
    #[init(crate::sb::members::BtreeBitmapGcSchedule::init())]
    pub maybe_schedule_btree_bitmap_gc: crate::util::workqueue::DelayedWork,

    pub counters: c::bch_fs_counters,
    #[c("struct delayed_work counters_recent")]
    #[pin]
    #[init(crate::sb::counters::RecentCounters::init())]
    pub counters_recent: crate::util::workqueue::DelayedWork,
    pub times: [c::bch2_time_stats; c::BCH_TIME_STAT_NR as usize],
    #[pin]
    #[init(c::bch_fs_errors::init())]
    pub errors: c::bch_fs_errors,

    /* C's CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS, which bcachefs.h defines from
     * these: rustc sees only the configuration */
    #[cfg(all(__KERNEL__, CONFIG_DEBUG_FS))]
    pub async_objs: [c::async_obj_list; c::BCH_ASYNC_OBJ_NR as usize],

    pub journal: c::journal,
    pub journal_replay_seq_start: u64,
    /*
     * Journal scrub: where its first flush range starts - the newest flush
     * before journal_replay_seq_start - while it runs; 0 otherwise.
     */
    pub journal_scrub_seq: u64,
    pub journal_replay_seq_end: u64,
    pub journal_entries: GenRadix<*mut c::journal_replay>,
    pub journal_entries_base_seq: u64,
    pub journal_keys: c::journal_keys,
    pub journal_iters: c::list_head,
    /// The blacklist, in memory: journal/seq_blacklist.rs's alone.
    #[pin]
    #[init(RustOnly::new(::pin_init::zeroed::<crate::journal::seq_blacklist::JournalSeqBlacklist>()))]
    pub(crate) journal_seq_blacklist: rust_only!(crate::journal::seq_blacklist::JournalSeqBlacklist),

    #[pin]
    #[init(c::bch_fs_recovery::init())]
    pub recovery: c::bch_fs_recovery,

    #[pin]
    #[init(c::bch_fs_logged_ops::init())]
    pub logged_ops: c::bch_fs_logged_ops,

    pub btree: c::bch_fs_btree,

    pub gc: c::bch_fs_gc,
    pub gc_gens: c::bch_fs_gc_gens,

    pub accounting: c::bch_accounting_mem,
    /// The replicas table, behind its own lock: alloc/replicas.rs's alone.
    #[pin]
    #[init(RustOnly::new(crate::new_lock!(crate::alloc::replicas::ReplicasCpu::new(), "&c->replicas")))]
    pub(crate) replicas: rust_only!(PercpuRwsemNoio<crate::alloc::replicas::ReplicasCpu>),
    pub disk_groups: c::DiskGroups,
    pub capacity: c::bch_fs_capacity,
    pub allocator: c::bch_fs_allocator,
    pub discards: c::bch_fs_discards,

    #[pin]
    #[init(c::bch_fs_snapshots::init())]
    pub snapshots: c::bch_fs_snapshots,

    pub write_error_lock: c::spinlock_t,
    /*
     * Use a dedicated wq for write ref holder tasks. Required to avoid
     * dependency problems with other wq tasks that can block on ref
     * draining, such as read-only transition.
     */
    pub write_ref_wq: *mut c::workqueue_struct,

    pub promote_wq: *mut c::workqueue_struct,
    #[c("struct semaphore __percpu *promote_limit")]
    pub promote_limit: *mut c::semaphore,

    pub io_clock: [c::io_clock; 2],
    pub clock_journal_res: c::journal_entry_res,
    pub rewind_limit_res: c::journal_entry_res,

    /* IO PATH */
    pub btree_update_wq: *mut c::workqueue_struct,
    pub bio_read: c::bio_set,
    pub bio_read_split: c::bio_set,
    pub bio_write: c::bio_set,
    pub replica_set: c::bio_set,
    pub bio_bounce_pages_lock: c::mutex,
    pub bio_bounce_bufs: c::mempool_t,
    pub nocow_locks: c::bucket_nocow_lock_table,
    pub update_table: c::rhltable,

    /*
     * The passphrase-derived key that unwraps the one below, when whoever
     * opened the filesystem handed it to us rather than leaving it in a
     * keyring for bch2_request_key() to go and find. Set before
     * bch2_fs_encryption_init() and zeroed as soon as it has been used -
     * the filesystem has no reason to keep it after that.
     */
    pub user_key: c::bch_key,
    pub user_key_set: bool,

    pub chacha20_key: c::bch_key,
    pub chacha20_key_set: bool,

    pub key_version: c::atomic64_t,

    /* MOVE.C */
    pub moving_context_list: c::list_head,
    pub moving_context_lock: c::mutex,

    /* Journal scrub: extents needing repair after recovery */
    pub scrub_journal_repairs: c::darray_scrub_journal_repair,
    pub scrub_journal_repairs_lock: c::mutex,

    /* Extents btree ranges from lost btree nodes, awaiting attribution */
    pub lost_extents_ranges: c::darray_lost_extents_range,
    pub lost_extents_ranges_lock: c::mutex,

    pub compress: c::bch_fs_compress,
    pub reconcile: c::bch_fs_reconcile,
    pub copygc: c::bch_fs_copygc,
    pub ec: c::bch_fs_ec,

    /// gc's count of references to indirect extents: reflink.rs's alone.
    #[pin]
    #[init(RustOnly::new(crate::data::reflink::ReflinkGc::new()))]
    pub(crate) reflink_gc: rust_only!(crate::data::reflink::ReflinkGc),

    #[cfg(not(NO_BCACHEFS_FS))]
    pub vfs: c::bch_fs_vfs,
    #[cfg(not(NO_BCACHEFS_FS))]
    pub fdm_table: c::fdm_hash,

    /// The in-memory quotas: quota.rs's alone.
    #[pin]
    #[init(RustOnly::new(c::Quotas::new()))]
    pub(crate) quotas: rust_only!(c::Quotas),

    /* DEBUG JUNK */

    #[cfg(CONFIG_DEBUG_FS)]
    pub fs_debug_dir: *mut c::dentry,
    #[cfg(CONFIG_DEBUG_FS)]
    pub btree_debug_dir: *mut c::dentry,
    #[cfg(CONFIG_DEBUG_FS)]
    pub async_obj_dir: *mut c::dentry,
    #[cfg(CONFIG_DEBUG_FS)]
    pub btree_debug: [c::btree_debug; c::BTREE_ID_NR as usize],
}

/// C's name, as FFI declarations spell it: *mut c::bch_fs.
pub type bch_fs = Fs;
