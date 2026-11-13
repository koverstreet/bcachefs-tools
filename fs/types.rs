// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::alloc::discard::{do_discards_fast, do_invalidates_work};
use crate::init::error::io_error_work;
use crate::init::rw::read_only_work;
use crate::cstructs::c;
use crate::new_lock;
use crate::util::alloc::KVVec;
use crate::data_structures::bitmap::AtomicBitflags;
use crate::util::kernel::WRITE;
use crate::util::kobject::Kobject;
use crate::util::projections::Volatile;
use crate::util::bio::BioSet;
use crate::util::late::Late;
use crate::util::place::Placed;
use crate::util::refcount::AtomicRef;
use crate::util::workqueue::{ReffedWork, Workqueue, WqFlags};
use crate::locking::{Locked, LockedBy, MutexNoio, PercpuRwsemNoio};
use crate::util::percpu::{Counters, PerCpu};
use crate::data_structures::genradix::SharedGenRadix;
use crate::types::{bits_to_longs};
use crate::util::rcu::RcuHead;
use crate::util::ffi::c_char;
use core::ffi::{c_int, c_uint, c_ushort};
use core::sync::atomic::{AtomicI32, AtomicI64, AtomicPtr, AtomicU16, AtomicU32, AtomicU64};
use cstruct_macros::{c_name, bch_enum, placed, CStruct, Fieldwise, Init, Late, TypeInfo};
use pin_init::pin_data;
use crate::format::{Bpos, Btree};

#[bch_enum(closed)]
#[prefix(BCH_TIME_)]
#[nr]
#[repr(u32)]
pub enum bch_time_stats {
    /// Allocate memory in the btree node cache for a new btree node
    btree_node_mem_alloc,
    /// Split a full btree node into two new nodes
    btree_node_split,
    /// Compact a full btree node on disk
    btree_node_compact,
    /// Merge two adjacent btree nodes
    btree_node_merge,
    /// Sort and resort entire btree nodes in memory, after reading from
    /// disk or for compacting
    btree_node_sort,
    /// Read btree nodes from disk
    btree_node_read,
    /// Post-read btree node processing
    btree_node_read_done,
    /// Write btree node to disk
    btree_node_write,
    /// Foreground time for topology-changing btree updates (splits,
    /// compactions, merges); roughly corresponds to lock held time
    btree_interior_update_foreground,
    /// Total time for topology-changing btree updates, including background
    /// transaction phase after new nodes are written
    btree_interior_update_total,
    /// scan btree node cache for eviction
    btree_node_cache_scan,
    /// scan btree key cache for eviction
    btree_key_cache_scan,
    /// Flush btree write buffer to btree
    btree_write_buffer_flush,
    /// Per-shard: queued to start of execution (workqueue scheduling delay)
    btree_write_buffer_flush_shard_sched_delay,
    /// Per-shard: actual flush work duration (wb_flush_sorted_range)
    btree_write_buffer_flush_shard_work,
    /// GC pass recalculating oldest generation numbers
    btree_gc,
    /// Core write path: allocate space, compress, encrypt, checksum, issue
    /// writes, update extents btree
    data_write,
    /// Core read path: look up extents btree, issue reads, checksum,
    /// decrypt, decompress
    data_read,
    /// Promote: write a cached copy of an extent to promote_target on read
    data_promote,
    /// Flush journal writes: cache flush to devices then FUA journal writes
    journal_flush_write,
    /// Non-flush journal writes, without cache flushes or FUA
    journal_noflush_write,
    /// Flush a journal sequence number to disk for sync, fsync, and bucket
    /// reuse
    journal_flush_seq,
    /// Flush btree journal pins
    journal_pin_flush_btree,
    /// Flush key cache journal pins
    journal_pin_flush_key_cache,
    /// Flush other journal pins
    journal_pin_flush_other,
    /// Write the superblock to every member device; held under sb_lock, so
    /// this is what other superblock writers wait behind
    sb_write,
    /// Blocked: journal reclaim not keeping up with reclaiming space
    blocked_journal_low_on_space,
    /// Blocked: journal pins (dirty btree nodes, key cache entries) not
    /// flushed fast enough
    blocked_journal_low_on_pin,
    /// Blocked: open buckets running low, throttling new journal work so
    /// reclaim can free them
    blocked_journal_low_on_open_buckets,
    /// Blocked: too many journal writes in flight
    blocked_journal_max_in_flight,
    /// Blocked: too many journal entries open, not yet closed for writing
    blocked_journal_max_open,
    /// Blocked: waiting for write buffer flush
    blocked_journal_blocked,
    /// Blocked: writer hit journal_full (no room in current entry, reclaim
    /// not keeping up)
    blocked_journal_full,
    /// Blocked: writer hit journal_pin_full (pin fifo full, btree node /
    /// key cache flushers not keeping up)
    blocked_journal_pin_full,
    /// Blocked: writer hit journal_buf_enomem (preallocated data buffer not
    /// topped up)
    blocked_journal_buf_enomem,
    /// Blocked: writer hit journal_stuck (10s timeout fired in slowpath
    /// wait)
    blocked_journal_stuck,
    /// Blocked: waiting for key cache flush
    blocked_key_cache_flush,
    /// Blocked: bucket allocation waiting, copygc or allocator thread not
    /// keeping up
    blocked_allocate,
    /// Blocked: all open bucket handles in use
    blocked_allocate_open_bucket,
    /// Blocked: write buffer full
    blocked_write_buffer_full,
    /// Blocked: writeback throttle
    blocked_writeback_throttle,
    /// Nocow lock contention
    nocow_lock_contended,
    /// Blocked: discard worker waiting for journal flush to advance
    /// rewind_seq and release buckets
    blocked_discard_journal_flush,
}

/* Number of nodes btree coalesce will try to coalesce at once */
pub const GC_MERGE_NODES: u32 = 4;

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Fieldwise)]
#[fieldwise(Counters::wrapping_add(rhs), Counters::clamp_nonnegative)]
pub struct io_count {
    pub sectors: [[u64; c::DataType::NR as usize]; 2],
}

#[bch_enum(closed)]
#[prefix(BCH_DEV_READ_REF_)]
#[nr]
#[repr(u32)]
pub enum bch_dev_read_ref {
    rust,
    bch2_online_devs,
    read_fua_test,
    journal_read,
    fs_journal_alloc,
    fs_resize_on_mount,
    btree_node_read,
    btree_node_read_all_replicas,
    btree_node_scrub,
    btree_node_write,
    btree_node_scan,
    btree_verify_replicas,
    io_read,
}

#[bch_enum(closed)]
#[prefix(BCH_DEV_WRITE_REF_)]
#[nr]
#[repr(u32)]
pub enum bch_dev_write_ref {
    rust,
    journal_write,
    journal_discard,
    discard_bucket,
    discard_one_bucket_fast,
    do_invalidates,
    nocow_flush,
    io_write,
    ec_bucket_zero,
}

/// A bucket_bitmap's words: replaced by a copy when the device grows.
pub type BucketBitmapWords = crate::util::rcu::RcuArray<crate::data_structures::bitmap::AtomicULong>;

crate::published! {
    /// A bucket_bitmap's bits: read without its lock - by copygc, while a
    /// check sets and frees them - so they're atomic words under RCU.
    #[derive(CStruct)]
    pub struct BucketBits {
        pub buckets: c::BucketBitmapWords,
        /// Bits set
        pub nr:      crate::util::projections::Volatile<u64>,
    }
}

/// One bit per bucket of a device, allocated when the first is set:
/// check_extents_to_backpointers' buckets whose backpointers don't add up, and
/// those with none. The lock serializes setting, growing and freeing; C calls
/// Rust for all of it.
#[pin_data]
#[repr(C)]
#[derive(CStruct, Init)]
pub struct bucket_bitmap {
    #[pin]
    #[init(crate::new_lock!((), ::pin_init::zeroed(), "&ca->bucket_bitmap.lock"))]
    pub bits: Locked<c::mutex, (), BucketBits>,
}

#[bch_enum(closed)]
#[prefix(BCH_DEV_)]
#[repr(u32)]
pub enum bch_dev_flags {
    /// a write completed that the device may still hold in its volatile
    /// cache; journal_write_preflush() clears it when it flushes the
    /// device.
    unflushed_writes,
}

pub const CONGESTED_MAX: u32 = 1024;

#[pin_data(PinnedDrop)]
#[repr(C)]
#[c_name(bch_dev)]
#[derive(CStruct, TypeInfo, Init, RcuHead)]
#[init_args(fs: &crate::fs::Fs)]
pub struct Dev {
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    pub ref_: core::sync::atomic::AtomicIsize,
    /* debug builds' ref checks, from any thread: members.h's and DevRef's */
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    pub dying: crate::util::projections::Volatile<bool>,
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    pub last_put: crate::util::projections::Volatile<core::ffi::c_ulong>,
    #[cfg(not(CONFIG_BCACHEFS_DEBUG))]
    #[pin]
    #[init(crate::util::refcount::PercpuRef::new())]
    pub ref_: crate::util::refcount::PercpuRef,
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
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
    #[init(AtomicRef::new(()))]
    pub ref_outer: AtomicRef<()>,
    #[pin]
    #[init(::pin_init::pin_init_array_from_fn(|_| crate::util::refcount::PercpuRef::new_dead::<()>()))]
    pub io_ref: [crate::util::refcount::PercpuRef; 2],

    /// Its filesystem, from the start, attached or not: bch2_dev_add()
    /// writes its superblock before it has a slot, and what fails before then
    /// reaches the filesystem through it. A device is freed before its
    /// filesystem - bch2_dev_free(), in the filesystem's teardown, drains
    /// ref_outer's holders first - so the filesystem outlives every use.
    #[init(unsafe { crate::util::backref::BackRef::new(fs) })]
    pub fs: crate::util::backref::BackRef<crate::fs::Fs>,

    pub dev_idx: u8,
    /*
     * Device is being removed and its alloc info and stripe pointers are
     * about to be deleted: new references must not be created. Checked by
     * __ec_stripe_create(), which may hold pre-invalidation copies of
     * stripe pointers; set by bch2_dev_remove() before the data drop,
     * cleared if removal fails.
     */
    pub removing: crate::util::projections::Volatile<bool>,
    /*
     * Cached version of this device's member info from superblock
     * Committed by bch2_write_super() -> bch_fs_mi_update(): written under
     * sb_lock, read without it
     */
    pub mi: crate::util::projections::Volatile<c::bch_member_cpu>,
    pub btree_allocated_bitmap_gc: crate::util::projections::Volatile<u64>,
    pub errors: [AtomicI64; c::bch_member_error_type::NR as usize],
    /// Written racing C's plain reads and stores, as C's are: atomic here,
    /// so Rust's are through a shared bch_dev.
    pub write_errors_start: crate::data_structures::bitmap::AtomicULong,
    pub flags: AtomicBitflags<c::bch_dev_flags>,

    pub uuid: c::__uuid_t,
    pub name: [c_char; c::BDEVNAME_SIZE as usize],
    #[pin]
    #[init(unsafe { Kobject::new::<Dev>() })]
    pub(crate) kobj: Placed<Kobject, crate::at!(Dev.kobj)>,

    /// Its superblock, sb_lock's - through Fs::dev_sb() - and the block
    /// device it's on, fixed while it's attached.
    pub disk_sb: LockedBy<c::bch_sb_io, c::bch_sb_bdev, crate::at!(Fs.sb)>,
    /// Its superblock's layout, for what can't take sb_lock to read disk_sb -
    /// whose buffer write_super can reallocate: the allocator, steering clear
    /// of superblock buckets. Copied as it's attached, and as it's edited -
    /// DevSbMut::edit_layout(), under sb_lock.
    pub sb_layout: crate::util::projections::Volatile<c::bch_sb_layout>,
    #[pin]
    #[init(None)]
    pub(crate) sb_read_scratch: Option<crate::util::alloc::KBox<crate::sb::io::SbReadScratch>>,
    pub dev: c::dev_t,

    pub self_mask: c::bch_devs_mask,

    /*
     * Buckets:
     * Per-bucket arrays are protected by either rcu_read_lock or
     * state_lock, for device resize.
     */
    pub buckets_gc: SharedGenRadix<c::bucket>,
    pub bucket_gens: LockedBy<(), c::BucketGensArray, crate::at!(Fs.state_lock)>,
    /* allocated once, by migrate, and read without a lock */
    pub buckets_nouse: LockedBy<(), c::BucketBitmapWords, crate::at!(Fs.state_lock)>,

    #[pin]
    #[init(c::bucket_bitmap::init())]
    pub bucket_backpointer_mismatch: c::bucket_bitmap,
    #[pin]
    #[init(c::bucket_bitmap::init())]
    pub bucket_backpointer_empty: c::bucket_bitmap,

    pub usage: PerCpu<c::bch_dev_usage_full>,

    /* Allocator: */
    /* where each btree_bitmap kind of allocation scans from: racy, as in C */
    pub alloc_cursor: [crate::util::projections::Volatile<u64>; 3],

    /*
     * Incremented by alloc_wake() at every site that wakes
     * freelist_wait for a specific device. Waiters on
     * c->allocator.freelist_wait snapshot the counters for the devices
     * they need at park time and compare on wake: if none have advanced,
     * the wake didn't concern this waiter and it re-parks without the
     * full alloc retry. Fs-wide wakes bump allocator.wake_all_counter
     * instead (see alloc_wake_all()).
     */
    pub alloc_wake_counter: AtomicI32,

    /* Buckets copygc has queued for evacuation on this device: */
    pub copygc_in_flight: AtomicI32,

    /* open buckets still free: under freelist_lock, read without it */
    pub nr_open_buckets: crate::util::projections::Volatile<c_uint>,
    /* on the partial list: written under freelist_lock */
    pub nr_partial_buckets: crate::util::projections::Volatile<c_uint>,
    pub nr_btree_reserve: c_uint,

    /* this device's share of c->capacity.reserved, which is their sum:
     * recalc_capacity()'s, under state_lock, read without it */
    pub reserved_sectors: crate::util::projections::Volatile<u64>,

    #[pin]
    pub(crate) invalidate_work:
        ReffedWork<do_invalidates_work, (crate::field!(io_ref[WRITE]), crate::field!(fs->writes))>,

    #[pin]
    pub(crate) discard_fast_work:
        ReffedWork<do_discards_fast, (crate::field!(io_ref[WRITE]), crate::field!(fs->writes))>,
    /// Buckets queued for fast discard: alloc/discard.rs's alone.
    #[pin]
    #[init(new_lock!(KVVec::new()))]
    pub(crate) discard_fast: Locked<c::mutex, KVVec<u64>>,
    /// Discards in flight on it, by every worker: a futex word, a worker at
    /// the limit sleeping on it.
    #[pin]
    #[init(core::sync::atomic::AtomicU8::new(0))]
    pub(crate) discards_in_flight: core::sync::atomic::AtomicU8,
    /// c->devs[]'s RCU readers may still have it, and take its io refs, which
    /// its drop exits: dropped a grace period on, in process context.
    #[pin]
    #[rcu_head]
    #[init(crate::util::rcu::RcuDropWorkMember::default())]
    pub(crate) _rcu: crate::util::rcu::RcuDropWorkMember<crate::util::rcu::RcuWorkHead>,

    pub rebalance_work: AtomicI64,

    #[pin]
    pub journal: c::journal_device,
    pub prev_journal_sector: Volatile<u64>,

    #[pin]
    pub(crate) io_error_work: ReffedWork<io_error_work, crate::field!(ref_outer)>,

    /* The rest of this all shows up in sysfs */
    pub cur_latency: [AtomicI64; 2],
    #[pin]
    #[init(::pin_init::pin_init_array_from_fn(|_| c::bch2_time_stats_quantiles::init()))]
    pub io_latency: [c::bch2_time_stats_quantiles; 2],

    pub congested: AtomicI32,
    /* written racily, by bch2_latency_acct() */
    pub congested_last: AtomicU64,

    pub io_done: PerCpu<c::io_count>,
}

/*
 * initial_gc_unfixed
 * error
 * topology error
 */
#[bch_enum(closed)]
#[prefix(BCH_FS_)]
#[repr(u32)]
pub enum bch_fs_flags {
    new_fs,
    start_begun,
    started,
    clean_recovery,
    btree_running,
    accounting_replay_done,
    may_go_rw,
    scrub_journal,
    may_upgrade_downgrade,
    rw,
    rw_init_done,
    was_rw,
    stopping,
    emergency_ro,
    going_ro,
    write_disable_complete,
    clean_shutdown,
    in_recovery,
    running_recovery_passes,
    in_fsck,
    initial_gc_unfixed,
    error,
    topology_error,
    errors_fixed,
    errors_fixed_silent,
    errors_not_fixed,
    no_invalid_checks,
    discard_mount_opt_set,
    sb_dirty,
    all_devs_rw,
}

#[cfg(CONFIG_DEBUG_FS)]
#[placed(Fs.btree_debug[], index = id: c_uint)]
#[pin_data]
#[repr(C)]
#[derive(CStruct, Init)]
pub struct btree_debug {}

pub const BCH_LINK_MAX: u32 = c::U32_MAX;

/// The filesystem: struct bch_fs, to C. Rust allocates it and frees it -
/// alloc(), its kobject's release - so its Rust-typed fields get their
/// initializers, from Fs::init(), and their Drop; init_from() and the
/// subsystems' inits and exits do the rest.
/// Borrowed by Rust as &Fs; OpenFs owns one opened from userspace.
#[pin_data]
#[repr(C)]
#[c_name(bch_fs)]
#[derive(CStruct, TypeInfo, Init, Late)]
#[init_args(options: &c::bch_opts)]
pub struct Fs {
    pub cl: c::closure,

    #[pin]
    #[init(unsafe { Kobject::new::<Fs>() })]
    pub(crate) kobj: Placed<Kobject, crate::at!(Fs.kobj)>,
    pub flags: AtomicBitflags<c::bch_fs_flags>,

    #[init(-1)]
    pub minor: c_int,
    pub chardev: *mut c::device,
    pub vfs_sb: *mut c::super_block,
    /// What its devices are claimed for: the open's, from alloc()
    #[pin]
    #[init(None)]
    pub(crate) holder: Option<crate::util::alloc::Arc<crate::sb::io::Holder>>,
    pub dev: c::dev_t,
    pub name: [c_char; 40],

    /// This, stdio_filter and stdio_user_only: set as a redirect attaches and
    /// detaches - the status fd's, online fsck's - and read by every print,
    /// on any thread. stdio published last, Release, and read Acquire.
    pub stdio: AtomicPtr<c::stdio_redirect>,
    pub stdio_filter: AtomicPtr<c::task_struct>,
    /*
     * Online fsck redirects everything it prints; mounting doesn't - a boot
     * splash wants the questions and the hard errors, not a running
     * commentary of IO errors. Set this and the redirect carries only what
     * bch2_print_str_user() sends.
     */
    pub stdio_user_only: core::sync::atomic::AtomicBool,
    /*
     * Someone is polling BCH_IOCTL_RECOVERY_STATUS on that redirect, so
     * they're drawing progress themselves and progress indicators keep off
     * the console entirely. Set by the first such ioctl, cleared when the
     * channel detaches - a mount that asks for a status fd but never polls
     * still gets its progress in dmesg.
     */
    pub stdio_progress_reader: core::sync::atomic::AtomicBool,
    /// These two and recovery_task: C's plain fields, atomic here, so Rust
    /// writes them through a shared Fs.
    pub loglevel: AtomicU32,
    pub prev_loglevel: AtomicU32,
    /*
     * Certain operations are only allowed in single threaded mode, during
     * recovery, and we want to assert that this is the case:
     */
    pub recovery_task: AtomicPtr<c::task_struct>,

    /* ro/rw, add/remove/resize devices: */
    #[pin]
    #[init(new_lock!((), "&c->state_lock"))]
    pub state_lock: Placed<Locked<c::rw_semaphore>, crate::at!(Fs.state_lock)>,

    /* Counts outstanding writes, for clean transition to read-only */
    #[init(crate::util::alloc::init_or(
        crate::util::refcount::PercpuRef::new_dead::<crate::init::rw::WritesDisabled>(),
        c::bch_errcode::BCH_ERR_ENOMEM_fs_other_alloc))]
    #[pin]
    pub writes: crate::util::refcount::PercpuRef,

    /*
     * Analagous to c->writes, for asynchronous ops that don't necessarily
     * need fs to be read-write
     */
    #[pin]
    #[init(AtomicRef::new(()))]
    pub(crate) ro_ref: Placed<AtomicRef<()>, crate::at!(Fs.ro_ref)>,
    /// Woken as it starts and as it stops
    #[pin]
    #[init(crate::util::futex::EventCount::new())]
    pub(crate) ro_ref_events: crate::util::futex::EventCount,
    #[pin]
    pub(crate) read_only_work: ReffedWork<read_only_work, crate::field!(ro_ref)>,

    /// Written under state_lock: StateLockWrite::devs().
    pub devs: LockedBy<(), [crate::util::rcu::RcuPtr<crate::util::refcount::ARef<Dev>>; c::BCH_SB_MEMBERS_MAX as usize],
                       crate::at!(Fs.state_lock)>,
    pub devs_online: c::AtomicDevsMask,
    pub devs_removed: c::AtomicDevsMask,
    pub devs_rotational: c::AtomicDevsMask,

    /// The options: read by anyone, an option at a time - written under
    /// sb_lock, which publishes them - Fs::opts_writer().
    #[init(LockedBy::new((), (*options).into()))]
    pub opts: LockedBy<(), c::PublishedOpts, crate::at!(Fs.sb)>,
    #[pin]
    #[init(new_lock!((), "&c->opt_change_lock"))]
    pub opt_change_lock: Locked<c::mutex>,
    /// Odd while options change, bumped under opt_change_lock by
    /// OptChange, taking it and dropping it; read without it.
    pub opt_change_cookie: crate::util::projections::Volatile<u32>,
    /// The options the mount set: what show_options lists. An option set
    /// since is no longer one.
    pub mount_opts: c::AtomicOptsMask,

    /// The superblock, c->disk_sb, behind sb_lock - Fs::sb_lock() is the way
    /// in - and what of it is read without the lock, c->sb, which it
    /// publishes.
    #[pin]
    #[init(crate::new_lock!(Default::default(), ::pin_init::zeroed(), "&c->sb_lock"))]
    pub sb: Placed<MutexNoio<c::bch_fs_sb, c::bch_sb_cpu>, crate::at!(Fs.sb)>,
    pub incompat_versions_requested: crate::data_structures::bitmap::AtomicBitArray<{
        bits_to_longs(c::BCH_VERSION_MINOR(c::bcachefs_metadata_version_current as u64) as usize) }>,
    pub cf_encoding: AtomicPtr<c::unicode_map>,

    #[init((options.block_size >> 9).ilog2() as c_ushort)]
    pub block_bits: c_ushort, /* ilog2(block_size) */

    /*
     * shard → preferred CPU mapping for wake_cpu hinting from
     * bch2_trans_begin(): each shard's worth of btree-node working set
     * gravitates to a fixed CPU's L1/L2.
     */
    pub inode_shard_cpu: [AtomicU16; 256],

    #[pin]
    #[init(crate::sb::members::BtreeBitmapGcSchedule::init())]
    pub maybe_schedule_btree_bitmap_gc: crate::util::workqueue::DelayedWork,

    pub counters: c::bch_fs_counters,
    #[pin]
    #[init(crate::sb::counters::RecentCounters::init())]
    pub counters_recent: crate::util::workqueue::DelayedWork,
    #[pin]
    #[init(::pin_init::pin_init_array_from_fn(|_| c::bch2_time_stats::init()))]
    pub times: [c::bch2_time_stats; c::bch_time_stats::NR as usize],
    #[pin]
    #[init(c::bch_fs_errors::init())]
    pub errors: c::bch_fs_errors,

    /* What's in flight, for debugfs: debug/async_objs.rs */
    #[pin]
    pub async_objs: [c::async_obj_list; c::bch_async_obj_lists::NR as usize],

    #[pin]
    #[late]
    pub journal: c::journal,
    pub journal_replay_seq_start: Volatile<u64>,
    /*
     * Journal scrub: where its first flush range starts - the newest flush
     * before journal_replay_seq_start - while it runs; 0 otherwise.
     */
    pub journal_scrub_seq: Volatile<u64>,
    pub journal_replay_seq_end: Volatile<u64>,
    pub journal_entries_base_seq: Volatile<u64>,
    #[pin]
    #[init(AtomicRef::pin_init(c::journal_keys::init()))]
    pub journal_keys: Placed<AtomicRef<c::journal_keys>, crate::at!(Fs.journal_keys)>,
    /// The blacklist, in memory: journal/seq_blacklist.rs's alone.
    #[pin]
    #[init(::pin_init::zeroed::<crate::journal::seq_blacklist::JournalSeqBlacklist>())]
    pub(crate) journal_seq_blacklist: crate::journal::seq_blacklist::JournalSeqBlacklist,

    #[pin]
    pub recovery: c::bch_fs_recovery,

    #[pin]
    #[init(c::bch_fs_logged_ops::init())]
    pub logged_ops: c::bch_fs_logged_ops,

    #[pin]
    #[placed_args(options)]
    #[late]
    pub btree: c::bch_fs_btree,

    #[pin]
    #[init(c::bch_fs_gc::init())]
    pub gc: c::bch_fs_gc,
    pub gc_gens: c::bch_fs_gc_gens,

    /// The in-memory accounting table, under mark_lock.
    #[pin]
    #[init(LockedBy::new(crate::alloc::accounting_mem::Table::default(), ()))]
    pub(crate) accounting: LockedBy<crate::alloc::accounting_mem::Table, (), c::MarkLockAt>,
    /// The replicas table, behind its own lock: alloc/replicas.rs's alone.
    #[pin]
    #[init(crate::new_lock!(crate::alloc::replicas::ReplicasCpu::new(), "&c->replicas"))]
    pub(crate) replicas: PercpuRwsemNoio<crate::alloc::replicas::ReplicasCpu>,
    pub disk_groups: LockedBy<(), c::DiskGroups, crate::at!(Fs.sb)>,
    #[pin]
    #[init(c::bch_fs_capacity::init())]
    pub capacity: c::bch_fs_capacity,
    #[pin]
    pub allocator: c::bch_fs_allocator,
    #[pin]
    pub discards: c::bch_fs_discards,

    #[pin]
    pub snapshots: c::bch_fs_snapshots,

    #[pin]
    #[init(new_lock!((), "&c->write_error_lock"))]
    pub write_error_lock: Locked<c::spinlock_t>,
    /*
     * Use a dedicated wq for write ref holder tasks. Required to avoid
     * dependency problems with other wq tasks that can block on ref
     * draining, such as read-only transition.
     */
    #[late(Workqueue::new(c"bcachefs_write_ref", WqFlags::FREEZABLE | WqFlags::PERCPU, 0), ENOMEM_fs_other_alloc)]
    pub write_ref_wq: Late<Workqueue>,

    #[late(Workqueue::new(c"bcachefs_promotes", WqFlags::FREEZABLE | WqFlags::PERCPU, 2), ENOMEM_fs_other_alloc)]
    pub promote_wq: Late<Workqueue>,
    /* Promotes in flight, against their limit: data/promote.rs */
    pub promotes_in_flight: core::sync::atomic::AtomicU32,

    #[pin]
    #[init(::pin_init::pin_init_array_from_fn(|_| c::io_clock::init()))]
    pub io_clock: [c::io_clock; 2],
    pub super_entries_journal_res: c::journal_entry_res,
    pub rewind_limit_res: c::journal_entry_res,

    /* IO PATH */
    #[late(Workqueue::new(c"bcachefs",
                          WqFlags::HIGHPRI | WqFlags::FREEZABLE | WqFlags::MEM_RECLAIM | WqFlags::UNBOUND, 512),
           ENOMEM_fs_other_alloc)]
    pub btree_update_wq: Late<Workqueue>,
    #[pin]
    #[init(BioSet::new(1, true), ENOMEM_bio_read_init)]
    pub bio_read: BioSet<crate::data::read::Rbio>,
    #[pin]
    #[init(BioSet::new(1, true), ENOMEM_bio_read_split_init)]
    pub bio_read_split: BioSet<crate::data::read::Rbio>,
    #[pin]
    #[init(crate::data::read::retry_pool(), ENOMEM_fs_other_alloc)]
    pub read_retry_pool: crate::util::mempool::Mempool<crate::util::alloc::Kmalloc>,
    #[late(BioSet::new(1, true), ENOMEM_bio_write_init)]
    pub bio_write: Late<BioSet<c::bch_write_bio>>,
    #[late(BioSet::new(4, false), ENOMEM_bio_write_init)]
    pub replica_set: Late<BioSet<c::bch_write_bio>>,
    #[pin]
    #[init(crate::util::bio::pages::BouncePool::new(options), ENOMEM_bio_bounce_pages_init)]
    pub(crate) bio_bounce: crate::util::bio::pages::BouncePool,
    #[pin]
    #[init(crate::data::nocow_locking::NocowLocks::new())]
    pub(crate) nocow_locks: crate::data::nocow_locking::NocowLocks,
    /// The data updates in flight, by data_update_key.
    #[pin]
    #[init(::pin_init::zeroed::<crate::data::update::InFlightTable>())]
    pub(crate) update_table: crate::data::update::InFlightTable,

    pub key_version: AtomicI64,

    /* Journal scrub: extents needing repair after recovery */
    #[pin]
    #[init(crate::new_lock!(crate::util::alloc::KVVec::new()))]
    pub(crate) scrub_journal_repairs:
        Locked<c::mutex, crate::util::alloc::KVVec<crate::data::scrub_journal::Repair>>,

    /* Extents btree ranges from lost btree nodes, awaiting attribution: init/damage.rs */
    #[pin]
    #[init(crate::new_lock!(crate::util::alloc::KVVec::new()))]
    pub(crate) lost_extents_ranges: Locked<c::mutex, crate::util::alloc::KVVec<core::ops::Range<Bpos>>>,

    #[pin]
    #[init(::pin_init::zeroed::<crate::data::compress::Compress>())]
    pub(crate) compress: crate::data::compress::Compress,
    #[pin]
    pub reconcile: c::bch_fs_reconcile,
    #[pin]
    #[init(c::bch_fs_copygc::init())]
    #[late]
    pub copygc: c::bch_fs_copygc,
    #[pin]
    pub ec: c::bch_fs_ec,

    /// gc's count of references to indirect extents: reflink.rs's alone.
    #[pin]
    #[init(crate::data::reflink::ReflinkGc::new())]
    pub(crate) reflink_gc: crate::data::reflink::ReflinkGc,

    #[cfg(not(NO_BCACHEFS_FS))]
    #[pin]
    #[init(c::bch_fs_vfs::init())]
    #[late]
    pub vfs: c::bch_fs_vfs,
    #[cfg(not(NO_BCACHEFS_FS))]
    #[init(c::fdm_hash::init())]
    pub fdm_table: c::fdm_hash,

    /// The in-memory quotas: quota.rs's alone.
    #[pin]
    #[init(c::Quotas::new())]
    pub(crate) quotas: c::Quotas,

    /* DEBUG JUNK */

    #[cfg(CONFIG_DEBUG_FS)]
    pub fs_debug_dir: *mut c::dentry,
    #[cfg(CONFIG_DEBUG_FS)]
    pub btree_debug_dir: *mut c::dentry,
    #[cfg(CONFIG_DEBUG_FS)]
    pub async_obj_dir: *mut c::dentry,
    #[cfg(CONFIG_DEBUG_FS)]
    #[pin]
    pub btree_debug: [c::btree_debug; Btree::NR as usize],
}
