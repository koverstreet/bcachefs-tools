// SPDX-License-Identifier: GPL-2.0

//! Fs - struct bch_fs - its accessors and locks, and OpenFs, the handle that
//! exits a filesystem when dropped.

#![allow(non_camel_case_types)]

use crate::init::passes::{RecoveryPass, FsRecovery};
use crate::btree::{node::BtreeNode, FsBtree, check_allocations::FsGc};
use crate::c;
use crate::alloc::capacity::{FsCapacity, MarkLockAt};
use crate::alloc::discard::FsDiscards;
use crate::alloc::gc_gens::FsGcGens;
use crate::bcachefs_ioctl::ForceIf;
use crate::data::{copygc::FsCopygc, ec::FsEc, reconcile::FsReconcile};
use crate::debug::async_objs::{AsyncObjList, AsyncObjKind};
#[cfg(CONFIG_DEBUG_FS)]
use crate::debug::debug::BtreeDebug;
use crate::journal::journal::{Journal, JournalEntryRes};
use crate::opts::{AtomicOptsMask, Opts, OPT_SHOW_MOUNT_STYLE, OptFlags, PublishedOpts,
    PublishedOptsWriter};
use crate::posix::{logged_ops::FsLoggedOps, quota::Quotas};
use crate::sb::members_format::{Member, BCH_SB_MEMBERS_MAX};
use crate::snapshots::{FsSnapshots, subvolume::SubvolInum};
use crate::util::clock::IoClock;
#[cfg(kernel)]
use crate::vfs::FsVfs;
#[cfg(kernel)]
use crate::vfs::fdm::FdmHash;
use crate::errcode::{Errcode, ret_to_c, BchError};
use crate::alloc::foreground::{allocator_foreground_init, FsAllocator};
use crate::alloc::replicas::{can_write_with_devs, WriteCheck};
use crate::btree::{init::btree_init, journal_overlay::{journal_keys_put_initial, JournalKeys}};
use crate::data::checksum::encryption_init;
use crate::data::compress::compress_init;
use crate::data::ec::head::ec_exit;
use crate::data::reconcile::work::reconcile_exit;
use crate::debug::{sysfs::{FsDir, FS_GROUPS}, trace::events};
use crate::init::dev::{dev_exit, dev_in_fs, AtomicDevsMask, FsSb, SbHandle};
use crate::init::error::{free_fsck_errs, FsErrors};
use crate::init::registry::register;
use crate::posix::inode::shard_cpu_init;
use crate::journal::seq_blacklist::blacklist_table_initialize;
use crate::locking::{Exclusive, ExclusiveGuard, Guard, Locked, LockedBy, Mutex, MutexNoio, Noio, PercpuRwsemNoio, RwSem, Shared, SharedGuard};
use crate::{bch_verbose, prt};
use crate::sb::counters::{counters_init, FsCounters};
use crate::sb::io::{read_super, sb_to_fs, Holder, SbCpu, SbWriteFlags};
use crate::sb::members::{dev_to_text_short, fs_mi_field_upgrades, member_exists, DevKobjRef, DevPieces, SbMembersMut};
use crate::sb::version::prep_for_start;
use crate::util::{Printbuf, ToText};
use crate::util::alloc::{Arc, Vec};
use crate::util::refcount::{PercpuRef, RefGuard, WaitableRef, AtomicRef};
use crate::util::ffi::c_char;
use crate::os::kernel::READ;
use crate::os::sched;
use crate::os::kobject::{into_ref, GroupList, KobjOwner, Kobject};
use crate::os::module::ModuleRef;
use crate::util::log::{log_msg_start, print_lines, print_str, LogLevel, LogMsg};
use crate::util::maybe_rcu_alloc::MaybeRcuAlloc;
use crate::util::place::{PlacedDrop, Placed};
use crate::util::rcu::{read_lock, RcuPtrWriter};
use crate::util::late::Late;
use core::ffi::CStr;
use core::ops::AsyncFnOnce;
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::Ordering::Relaxed;
pub use crate::sb::members::DevRef;
use core::sync::atomic::{AtomicI64, AtomicPtr, AtomicU16, AtomicU32};
use cstruct_macros::{bch_enum, Init, Late, MaybeZeroable, TypeInfo};
use pin_init::pin_data;
use crate::data_structures::bitmap::AtomicBitflags;
use crate::init::rw::read_only_work;
use crate::new_lock;
use crate::os::bio::{Bio, BioSet};
use crate::os::unicode::UnicodeMap;
use crate::os::workqueue::{DelayedWork, ReffedWork, Workqueue, WqFlags};
use crate::sb::{counters::recent_counters, members::btree_bitmap_gc_schedule};
use crate::types::bits_to_longs;
use crate::util::{projections::Volatile, time_stats::{TimeStatsArray, TimeStats, TimeStat}};
#[cfg(kernel)]
use kernel::sync::SetOnce;
#[cfg(not(kernel))]
use bcachefs_shim::sync::SetOnce;
use crate::format::{Bpos, Btree, MemberState, MetadataVersion, SbFeature, Key, Sb,
    JsetEntryRewindLimit, SbCompat};

/// sb_lock, held: the superblock, and c->sb, which it publishes
pub type SbLockGuard<'a> =
    ExclusiveGuard<'a, Noio<Mutex>, FsSb, SbCpu, Exclusive, crate::at!(Fs.sb)>;

impl SbLockGuard<'_> {
    pub fn fs_member_mut(&mut self, dev_idx: u32) -> &mut Member {
        self.member_mut(dev_idx)
            .expect("a member, whole: members_v2_init() grew them at open")
    }

    /// The superblock, and @F of each of the filesystem's devices, at once.
    pub(crate) fn split_devs<F>(&mut self) -> (&mut FsSb, DevPieces<'_, crate::at!(Fs.sb), F>)
    where
        F: crate::util::place::Path<Dev>,
        F::To: crate::locking::AsLockedBy<L = crate::at!(Fs.sb)> + 'static,
    {
        let fs: *const Fs = self.root();
        /* the superblock is the lock's, apart from any device's */
        unsafe { (&mut **self, DevPieces::new(&*fs)) }
    }
}

pub struct StateLockRead<'a> {
    fs:     &'a Fs,
    _guard: SharedGuard<'a, RwSem, (), (), Shared, crate::at!(Fs.state_lock)>,
}

pub struct StateLockWrite<'a> {
    fs:     &'a Fs,
    _guard: ExclusiveGuard<'a, RwSem, (), (), Exclusive, crate::at!(Fs.state_lock)>,
}

/// Its guard: what state_lock covers is had through it.
impl<'a> core::ops::Deref for StateLockWrite<'a> {
    type Target = ExclusiveGuard<'a, RwSem, (), (), Exclusive, crate::at!(Fs.state_lock)>;

    fn deref(&self) -> &Self::Target {
        &self._guard
    }
}

impl<'a> StateLockWrite<'a> {
    pub(crate) fn fs(&self) -> &'a Fs {
        self.fs
    }

    /// c->devs[], as their writer has them.
    pub(crate) fn devs(&self) -> &[RcuPtrWriter<DevKobjRef>; BCH_SB_MEMBERS_MAX as usize] {
        self.writer::<crate::field!(devs)>()
    }
}

impl StateLockRead<'_> {
    pub fn recalc_capacity(&self) {
        crate::alloc::capacity::recalc_capacity(self.fs)
    }
}

pub use crate::init::dev::Dev;

/*
 * initial_gc_unfixed
 * error
 * topology error
 */
#[bch_enum(closed)]
#[repr(u32)]
pub enum FsFlag {
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

/// The filesystem: struct bch_fs, to C. Rust allocates it and frees it -
/// alloc(), its kobject's release - so its Rust-typed fields get their
/// initializers, from Fs::init(), and their Drop; init_from() and the
/// subsystems' inits and exits do the rest.
/// Borrowed by Rust as &Fs; OpenFs owns one opened from userspace.
#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, TypeInfo, Init, Late)]
#[init_args(options: &Opts)]
pub struct Fs {
    #[pin]
    #[init(unsafe { Kobject::new::<Fs>() })]
    pub(crate) kobj: Placed<Kobject, crate::at!(Fs.kobj)>,
    pub flags: AtomicBitflags<FsFlag>,

    /// Its control device, from online() to stop()
    #[cfg(kernel)]
    #[pin]
    #[init(crate::locking::Locked::new_const(None, ()))]
    pub(crate) chardev: crate::locking::Locked<crate::locking::ByteLock, Option<crate::init::chardev::CtlDev>>,
    #[cfg(kernel)]
    #[init(SetOnce::new())]
    pub vfs_sb: crate::os::super_block::MountedOn<Fs>,
    /// What its devices are claimed for: the open's, from alloc()
    #[pin]
    #[init(None)]
    pub(crate) holder: Option<crate::util::alloc::Arc<crate::sb::io::Holder>>,
    /* from init_early(), put by its release, after it's dropped */
    #[init(None)]
    pub(crate) module_ref: Option<crate::os::module::ModuleRef>,
    pub name: [c_char; 40],

    /// This, stdio_filter and stdio_user_only: set as a redirect attaches and
    /// detaches - the status fd's, online fsck's - and read by every print,
    /// on any thread. stdio published last, Release, and read Acquire.
    pub stdio: AtomicPtr<crate::os::thread_with_file::StdioRedirect>,
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
    pub state_lock: Placed<Locked<RwSem>, crate::at!(Fs.state_lock)>,

    /* Counts outstanding writes, for clean transition to read-only */
    #[init(crate::util::alloc::init_or(
        crate::util::refcount::PercpuRef::new_dead::<crate::init::rw::WritesDisabled>(),
        Errcode::ENOMEM_fs_other_alloc))]
    #[pin]
    pub writes: Placed<crate::util::refcount::PercpuRef, crate::at!(Fs.writes)>,

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
    pub devs: LockedBy<(), [crate::util::rcu::RcuPtr<crate::sb::members::DevKobjRef>; BCH_SB_MEMBERS_MAX as usize],
                       crate::at!(Fs.state_lock)>,
    pub devs_online: AtomicDevsMask,
    pub devs_removed: AtomicDevsMask,
    pub devs_rotational: AtomicDevsMask,

    /// The options: read by anyone, an option at a time - written under
    /// sb_lock, which publishes them - Fs::opts_writer().
    #[init(LockedBy::new((), (*options).into()))]
    pub opts: LockedBy<(), PublishedOpts, crate::at!(Fs.sb)>,
    #[pin]
    #[init(new_lock!((), "&c->opt_change_lock"))]
    pub opt_change_lock: Locked<Mutex>,
    /// Odd while options change, bumped under opt_change_lock by
    /// OptChange, taking it and dropping it; read without it.
    pub opt_change_cookie: crate::util::projections::Volatile<u32>,
    /// The options the mount set: what show_options lists. An option set
    /// since is no longer one.
    pub mount_opts: AtomicOptsMask,

    /// The superblock, c->disk_sb, behind sb_lock - Fs::sb_lock() is the way
    /// in - and what of it is read without the lock, c->sb, which it
    /// publishes.
    #[pin]
    #[init(crate::new_lock!(Default::default(), ::pin_init::zeroed(), "&c->sb_lock"))]
    pub sb: Placed<MutexNoio<FsSb, SbCpu>, crate::at!(Fs.sb)>,
    pub incompat_versions_requested: crate::data_structures::bitmap::AtomicBitArray<{
        bits_to_longs(MetadataVersion::CURRENT.minor() as usize) }>,
    #[init(SetOnce::new())]
    pub cf_encoding: SetOnce<UnicodeMap>,

    #[init((options.block_size >> 9).ilog2() as u16)]
    pub block_bits: u16, /* ilog2(block_size) */

    /*
     * shard → preferred CPU mapping for wake_cpu hinting from
     * bch2_trans_begin(): each shard's worth of btree-node working set
     * gravitates to a fixed CPU's L1/L2.
     */
    pub inode_shard_cpu: [AtomicU16; 256],

    #[pin]
    pub(crate) maybe_schedule_btree_bitmap_gc: DelayedWork<btree_bitmap_gc_schedule, crate::field!(ro_ref)>,

    #[pin]
    #[init(crate::util::alloc::init_or(FsCounters::init(), Errcode::ENOMEM_fs_counters_init))]
    pub counters: FsCounters,
    #[pin]
    pub(crate) counters_recent: DelayedWork<recent_counters, crate::field!(ro_ref)>,
    #[pin]
    #[init(TimeStatsArray::new())]
    pub times: TimeStatsArray<TimeStats, { TimeStat::NR as usize }>,
    #[pin]
    #[init(FsErrors::init())]
    pub errors: FsErrors,

    /* What's in flight, for debugfs: debug/async_objs.rs */
    #[pin]
    pub async_objs: [AsyncObjList; AsyncObjKind::NR as usize],

    #[pin]
    #[late]
    pub journal: Journal,
    pub journal_replay_seq_start: Volatile<u64>,
    /*
     * Journal scrub: where its first flush range starts - the newest flush
     * before journal_replay_seq_start - while it runs; 0 otherwise.
     */
    pub journal_scrub_seq: Volatile<u64>,
    pub journal_replay_seq_end: Volatile<u64>,
    pub journal_entries_base_seq: Volatile<u64>,
    #[pin]
    #[init(AtomicRef::pin_init(JournalKeys::init()))]
    pub journal_keys: Placed<AtomicRef<JournalKeys>, crate::at!(Fs.journal_keys)>,
    /// The blacklist, in memory: journal/seq_blacklist.rs's alone, rebuilt
    /// under sb_lock.
    pub(crate) journal_seq_blacklist: LockedBy<(), crate::journal::seq_blacklist::JournalSeqBlacklist, crate::at!(Fs.sb)>,

    #[pin]
    pub recovery: FsRecovery,

    #[pin]
    #[init(FsLoggedOps::init())]
    pub logged_ops: FsLoggedOps,

    #[pin]
    #[placed_args(options)]
    #[late]
    pub btree: FsBtree,

    #[pin]
    #[init(FsGc::init())]
    pub gc: FsGc,
    pub gc_gens: FsGcGens,

    /// The in-memory accounting table, under mark_lock.
    #[pin]
    #[init(LockedBy::new(crate::alloc::accounting_mem::Table::default(), ()))]
    pub(crate) accounting: LockedBy<crate::alloc::accounting_mem::Table, (), MarkLockAt>,
    /// The replicas table, behind its own lock: alloc/replicas.rs's alone.
    #[pin]
    #[init(crate::new_lock!(crate::alloc::replicas::ReplicasCpu::new(), "&c->replicas"))]
    pub(crate) replicas: PercpuRwsemNoio<crate::alloc::replicas::ReplicasCpu>,
    pub disk_groups: LockedBy<(), crate::alloc::disk_groups::DiskGroups, crate::at!(Fs.sb)>,
    #[pin]
    #[init(FsCapacity::init())]
    pub capacity: FsCapacity,
    #[pin]
    pub allocator: FsAllocator,
    #[pin]
    pub discards: FsDiscards,

    #[pin]
    pub snapshots: FsSnapshots,

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
    #[init(::pin_init::pin_init_array_from_fn(|_| IoClock::init()))]
    pub io_clock: [IoClock; 2],
    pub super_entries_journal_res: JournalEntryRes,
    pub rewind_limit_res: JournalEntryRes,

    /* IO PATH */
    #[late(Workqueue::new(c"bcachefs",
                          WqFlags::HIGHPRI | WqFlags::FREEZABLE | WqFlags::MEM_RECLAIM | WqFlags::UNBOUND, 512),
           ENOMEM_fs_other_alloc)]
    pub btree_update_wq: Late<Workqueue>,
    #[pin]
    #[init(crate::data::read::retry_pool(), ENOMEM_fs_other_alloc)]
    pub read_retry_pool: crate::util::mempool::Mempool,
    #[late(BioSet::new(1), ENOMEM_bio_write_init)]
    pub(crate) bio_write: Late<BioSet<Bio<crate::data::write::ExtentBuf>>>,
    #[pin]
    #[init(crate::os::bio::pages::BouncePool::new(options), ENOMEM_bio_bounce_pages_init)]
    pub(crate) bio_bounce: crate::os::bio::pages::BouncePool,
    #[pin]
    #[init(crate::data::nocow_locking::NocowLocks::new())]
    pub(crate) nocow_locks: crate::data::nocow_locking::NocowLocks,
    /// The data updates in flight, by DataUpdateKey.
    #[pin]
    #[init(::pin_init::zeroed::<crate::data::update::InFlightTable>())]
    pub(crate) update_table: crate::data::update::InFlightTable,

    pub key_version: AtomicI64,

    /* Journal scrub: extents needing repair after recovery */
    #[pin]
    #[init(crate::new_lock!(crate::util::alloc::Vec::new()))]
    pub(crate) scrub_journal_repairs:
        Locked<Mutex, crate::util::alloc::Vec<crate::data::scrub_journal::Repair>>,

    /* Extents btree ranges from lost btree nodes, awaiting attribution: init/damage.rs */
    #[pin]
    #[init(crate::new_lock!(crate::util::alloc::Vec::new()))]
    pub(crate) lost_extents_ranges: Locked<Mutex, crate::util::alloc::Vec<core::ops::Range<Bpos>>>,

    #[pin]
    #[init(::pin_init::zeroed::<crate::data::compress::Compress>())]
    pub(crate) compress: crate::data::compress::Compress,
    #[pin]
    pub reconcile: FsReconcile,
    #[pin]
    #[init(FsCopygc::init())]
    #[late]
    pub copygc: FsCopygc,
    #[pin]
    pub ec: FsEc,

    /// gc's count of references to indirect extents: reflink.rs's alone.
    #[pin]
    #[init(crate::data::reflink::ReflinkGc::new())]
    pub(crate) reflink_gc: crate::data::reflink::ReflinkGc,

    #[cfg(kernel)]
    #[pin]
    #[init(FsVfs::init())]
    #[late]
    pub vfs: FsVfs,
    #[cfg(kernel)]
    #[init(FdmHash::init())]
    pub fdm_table: FdmHash,

    /// The in-memory quotas: quota.rs's alone.
    #[pin]
    #[init(Quotas::new())]
    pub(crate) quotas: Quotas,

    /* DEBUG JUNK */

    #[cfg(CONFIG_DEBUG_FS)]
    pub fs_debug_dir: Option<crate::os::debugfs::DebugDir>,
    #[cfg(CONFIG_DEBUG_FS)]
    #[pin]
    pub btree_debug: [BtreeDebug; Btree::NR as usize],
}

impl Fs {
    pub fn as_ptr(&self) -> *mut Fs {
        self as *const Fs as *mut Fs
    }
}

// SAFETY: as C shares a bch_fs: every field is synchronized by its own lock,
// atomic or RCU
unsafe impl Sync for Fs {}

impl Fs {
    pub fn name(&self) -> &core::ffi::CStr {
        crate::util::ffi::cstr_in(&self.name)
    }

    /// Its control device's minor, -1 without one - as in userspace, which
    /// has no control devices.
    pub fn minor(&self) -> i32 {
        #[cfg(kernel)]
        return self.chardev.lock().as_ref().map_or(-1, |d| d.minor() as i32);
        #[cfg(not(kernel))]
        -1
    }

    pub fn opts(&self) -> &PublishedOpts {
        &self.opts
    }

    pub fn opts_writer<'a>(&'a self, sb: &'a mut SbLockGuard<'_>) -> &'a PublishedOptsWriter {
        sb.writer::<crate::field!(opts)>()
    }

    /// log2 of the block size, in 512-byte sectors
    pub fn block_bits(&self) -> u32 {
        self.block_bits as u32
    }

    /*
     * Shard index for inode-number allocation. We used to use the current CPU id,
     * but threads migrate across CPUs and the win from per-CPU allocator
     * separation evaporates — concurrent allocators end up sharing shards (and
     * fighting on the same alloc_cursor btree node) any time the scheduler
     * shuffles them onto the same core. Hashing the task's pid is stable per
     * thread, so concurrent allocators in different threads keep their separation
     * regardless of which CPU they're currently running on.
     */
    pub fn inode_shard_idx(&self) -> u64 {
        let bits = self.opts().shard_inode_numbers_bits.get();
        if bits == 0 {
            return 0;
        }
        crate::util::hash::hash_64(sched::current_pid() as u64, bits.into())
    }

    /// Never unset once set
    pub fn encryption_key(&self) -> Option<&Key> {
        self.sb.encryption_key.get()
    }

    /// # Safety
    /// @raw points to a live bch_fs, that outlives 'a.
    pub unsafe fn borrow_raw<'a>(raw: *mut Fs) -> &'a Fs {
        unsafe { &*raw }
    }

    pub fn version(&self) -> MetadataVersion {
        self.sb.version.get()
    }

    /// Below version() while an upgrade's migrations are still running
    pub fn version_upgrade_complete(&self) -> MetadataVersion {
        self.sb.version_upgrade_complete.get()
    }

    /// Topology repair dropped nodes from btree @id
    pub fn btree_lost_data(&self, id: Btree) -> bool {
        self.sb.btrees_lost_data.get() & (1u64 << id.0) != 0
    }

    /// An image from `bcachefs dump --sanitize`, which scrubs dirent names
    /// without rehashing them
    pub fn dirents_sanitized(&self) -> bool {
        self.sb.dirents_sanitized.get()
    }

    /// A mount is reading recovery progress from BCH_IOCTL_RECOVERY_STATUS,
    /// so progress indicators needn't print it
    pub fn stdio_progress_reader(&self) -> bool {
        self.stdio_progress_reader.load(Relaxed)
    }

    pub fn journal_cur_seq(&self) -> u64 {
        self.journal.lock.seq.load(core::sync::atomic::Ordering::Relaxed) as u64
    }

    pub fn sb_lock(&self) -> SbLockGuard<'_> {
        self.sb.lock()
    }

    pub fn holder(&self) -> &Arc<Holder> {
        self.holder.as_ref().expect("a filesystem's holder, from alloc()")
    }

    pub fn mark_lock(&self) -> &crate::util::place::Placed<PercpuRwsemNoio, MarkLockAt> {
        &self.capacity.mark_lock
    }

    pub fn mark_lock_assert_held(&self) {
        self.mark_lock().assert_held_any()
    }

    /// Devices and capacity don't change under it
    pub fn state_lock_read(&self) -> StateLockRead<'_> {
        StateLockRead { fs: self, _guard: self.state_lock.read() }
    }

    pub fn state_lock_write(&self) -> StateLockWrite<'_> {
        StateLockWrite { fs: self, _guard: self.state_lock.write() }
    }

    #[track_caller]
    pub fn write_super(&self, sb: &mut SbLockGuard<'_>) {
        let _ = self.write_super_ret(sb);
    }

    /// For a write only for a replicas entry: it may leave the slow disks behind
    #[track_caller]
    pub fn write_super_replicas(&self, sb: &mut SbLockGuard<'_>) {
        let _ = self.write_super_replicas_ret(sb);
    }

    /// Counted and traced, as bch_err_throw(); BchError::from() names an error
    /// without throwing it. Cold, as C's unlikely(ret).
    #[cold]
    pub fn err(&self, error: impl Into<BchError>) -> BchError {
        let e = error.into();
        assert!(e.raw() > 0, "throwing {e}, not an error");
        self.event_inc_trace(events::error_throw, |out| prt!(out, "{e}"));
        e
    }

    pub fn throw<T>(&self, error: Errcode) -> Result<T, BchError> {
        Err(self.err(error))
    }

    pub fn require<T>(&self, value: Option<T>, error: Errcode) -> Result<T, BchError> {
        match value {
            Some(v) => Ok(v),
            None    => self.throw(error),
        }
    }

    pub fn ensure(&self, condition: bool, error: Errcode) -> Result<(), BchError> {
        if condition {
            Ok(())
        } else {
            self.throw(error)
        }
    }

    pub fn btree_id_root(&self, id: u32) -> Option<&BtreeNode> {
        self.btree.cache.roots.node(id)
    }

    /// Btree @id's root error, published as the root is.
    pub fn btree_id_root_set_error(&self, id: u32, err: i32) {
        self.btree.cache.roots.lock().published().roots_error[id as usize].set(err as i16);
    }

    pub fn btree_id_nr_alive(&self) -> u32 {
        Btree::NR + self.btree.cache.roots.roots_nr_extra.get()
    }

    pub fn nr_devices(&self) -> u32 {
        self.sb.nr_devices.get() as u32
    }

    /// Everything committed before the call is on disk when it returns
    pub fn journal_flush(&self) -> Result<(), BchError> {
        self.journal.flush()
    }

    /// A logged op, resumed after a crash. Block granular: zeroing the rest of
    /// the last block is the caller's, as the VFS does in the page cache.
    pub fn truncate(&self, inum: SubvolInum, new_i_size: u64) -> Result<(), BchError> {
        crate::data::io_misc::truncate(self, inum, new_i_size, &mut 0)
    }

    pub fn request_incompat_feature(&self, version: MetadataVersion)
        -> Result<(), BchError>
    {
        if version <= self.sb.version_incompat.get() {
            return Ok(());
        }
        self.set_version_incompat(version)
    }

    pub fn feature(&self, feature: SbFeature) -> bool {
        self.sb.features.get().contains(feature.into())
    }

    pub fn compat(&self, compat: SbCompat) -> bool {
        self.sb.compat.get().contains(compat.into())
    }

    pub fn check_set_feature(&self, feature: SbFeature) {
        if !self.feature(feature) {
            self.set_feature(feature)
        }
    }

    pub fn casefold_enabled(&self) -> Result<(), BchError> {
        if !cfg!(CONFIG_UNICODE) {
            return self.throw(Errcode::no_casefolding_without_utf8);
        }
        if self.opts().casefold_disabled.get() != 0 {
            return self.throw(Errcode::casefolding_disabled);
        }
        Ok(())
    }

    #[track_caller]
    pub fn write_super_ret(&self, sb: &mut SbLockGuard<'_>) -> Result<(), BchError> {
        self.write_super_flags(sb, SbWriteFlags::empty())
    }

    /// c->sb and member info updated from the superblock, without writing it
    pub fn sb_update(&self, sb: &mut SbLockGuard<'_>) {
        crate::sb::io::sb_update(self, sb)
    }

    pub fn journal_flush_all_pins(&self) -> bool {
        self.journal.flush_pins(u64::MAX)
    }

    pub fn journal_flush_outstanding_pins(&self) -> bool {
        self.journal.flush_pins(self.journal_cur_seq())
    }

    pub fn block_bytes(&self) -> u64 {
        self.opts().block_size.get() as u64
    }

    pub fn block_sectors(&self) -> u32 {
        u32::from(self.opts().block_size.get()) >> 9
    }

    pub fn btree_sectors(&self) -> u32 {
        self.opts().btree_node_size.get() >> 9
    }

    pub fn btree_max_u64s(&self) -> usize {
        crate::btree::btree_max_u64s(self.opts().btree_node_size.get())
    }

    pub fn time_to_timespec(&self, time: i64) -> c::timespec64 {
        const NSEC_PER_SEC: i64 = 1_000_000_000;

        let sb = &self.sb;
        let time = time.wrapping_add(sb.time_base_lo.get() as i64);
        let units = sb.time_units_per_sec.get() as i64;

        let mut sec  = time / units;
        let mut nsec = (time % units) * sb.nsec_per_time_unit.get() as i64;

        // set_normalized_timespec64()
        while nsec >= NSEC_PER_SEC {
            nsec -= NSEC_PER_SEC;
            sec += 1;
        }
        while nsec < 0 {
            nsec += NSEC_PER_SEC;
            sec -= 1;
        }

        let mut t: c::timespec64 = pin_init::Zeroable::zeroed();
        t.tv_sec  = sec as _;
        t.tv_nsec = nsec as _;
        t
    }

    // tv_sec is the kernel's time64_t, or userspace's time_t - 32 bits on some
    // targets
    #[allow(clippy::unnecessary_cast)]
    pub fn timespec_to_time(&self, ts: c::timespec64) -> i64 {
        let sb = &self.sb;
        // C's (int) tv_nsec / nsec_per_time_unit: an unsigned division
        (ts.tv_sec as i64).wrapping_mul(sb.time_units_per_sec.get() as i64)
            .wrapping_add((ts.tv_nsec as i32 as u32 / sb.nsec_per_time_unit.get()) as i64)
            .wrapping_sub(sb.time_base_lo.get() as i64)
    }

    /// In the filesystem's time units.
    pub fn current_time(&self) -> u64 {
        self.timespec_to_time(crate::os::kernel::ktime_get_coarse_real_ts64()) as u64
    }

    pub fn set_loglevel(&self, level: u32) {
        self.loglevel.store(level, Relaxed);
    }
}

/// The filesystem @sbs are the devices of, with options @opts: Fs::init()'s
/// fields, the rest from init_from(). It takes the devices it attaches,
/// leaving their handles zeroed; on failure, it's exited.
fn alloc(sbs: &mut Vec<SbHandle>, opts: &Opts, holder: Arc<Holder>,
         user_key: Option<&Key>, out: &mut Printbuf) -> Result<OpenFs, BchError> {
    let mut b = crate::util::alloc::alloc_pinned_or(Fs::init(opts), Errcode::ENOMEM_fs_alloc)?;
    // nothing else reaches it till init_from() registers it; if init_early()
    // fails, the OpenFs exits it
    let early = b.as_pin_mut().init_early(sbs, opts, holder);
    let fs = OpenFs::from_ref(into_ref(b));
    early?;
    fs.init_from(sbs, user_key, out)?;
    Ok(fs)
}

/// The default encoding, utf8 12.1.0: UNICODE_AGE()'s major, minor and rev
/// bytes
#[cfg(CONFIG_UNICODE)]
const BCH_FS_DEFAULT_UTF8_ENCODING: u32 = (12 << 16) | (1 << 8);

impl Fs {
    /// What's set while nothing else can reach it: its @holder, its superblock,
    /// and its name.
    fn init_early(mut self: Pin<&mut Self>, sbs: &Vec<SbHandle>, opts: &Opts,
                  holder: Arc<Holder>) -> Result<(), BchError> {
        *self.as_mut().project().holder = Some(holder);
        self.stdio.store(opts.stdio as usize as *mut _, Relaxed);
        self.recovery_task.store(sched::current(), Relaxed);
        *self.as_mut().project().module_ref = Some(ModuleRef::get());

        allocator_foreground_init(&self);

        {
            let mut sb = self.sb_lock();
            sb_to_fs(&self, &mut sb, sbs[0].sb())?;
            self.sb_resize(&mut sb).members_v2_init()
                .ok_or_else(|| self.err(Errcode::ENOSPC_sb_members_v2))?;
        }

        shard_cpu_init(&self);

        let mut name = Printbuf::new();
        if self.sb.multi_device.get() {
            prt!(&mut name, "{}", self.sb.user_uuid());
        } else {
            sbs[0].bdev().to_text(&mut name);
        }
        if name.allocation_failure() {
            return Err(Errcode::ENOMEM_fs_name_alloc.into());
        }
        // strscpy()
        let dst = self.as_mut().project().name;
        let len = name.as_bytes().len().min(dst.len() - 1);
        for (d, &s) in dst.iter_mut().zip(&name.as_bytes()[..len]) {
            *d = s as c_char;
        }
        dst[len] = 0;
        Ok(())
    }

    fn init_from(&self, sbs: &mut Vec<SbHandle>, user_key: Option<&Key>, out: &mut Printbuf)
        -> Result<(), BchError>
    {
        blacklist_table_initialize(self)?;
        btree_init(self)?;
        compress_init(self)?;
        counters_init(self);
        encryption_init(self, user_key)?;

        #[cfg(CONFIG_UNICODE)]
        if self.casefold_enabled().is_ok() {
            /* Default encoding until we can potentially have more as an option. */
            let Some(enc) = crate::os::unicode::UnicodeMap::load(BCH_FS_DEFAULT_UTF8_ENCODING) else {
                prt!(out, "Cannot load UTF-8 encoding for filesystem. Version: {}.{}.{}\n",
                     BCH_FS_DEFAULT_UTF8_ENCODING >> 16,
                     (BCH_FS_DEFAULT_UTF8_ENCODING >> 8) & 0xff,
                     BCH_FS_DEFAULT_UTF8_ENCODING & 0xff);
                return self.throw(Errcode::EINVAL_utf8_load_failed);
            };
            self.cf_encoding.populate(enc);
        }
        #[cfg(not(CONFIG_UNICODE))]
        if self.feature(SbFeature::casefolding) {
            prt!(out, "Cannot mount a filesystem with casefolding on a kernel without CONFIG_UNICODE\n");
            return self.throw(Errcode::EINVAL_casefolding_no_unicode);
        }

        for i in 0..self.nr_devices() {
            if !member_exists(self.sb_lock().sb(), i) {
                continue;
            }
            self.dev_alloc(i)?;
        }

        self.journal_entry_res_resize(&self.btree.root_journal_res,
            crate::btree::interior::roots_journal_u64s(Btree::NR));
        self.journal_entry_res_resize(&self.super_entries_journal_res,
            crate::journal::write::PREP_ENTRIES_U64S as u32);
        self.journal_entry_res_resize(&self.rewind_limit_res,
            crate::journal::entries::entry_u64s::<JsetEntryRewindLimit>() as u32);

        {
            let state = self.state_lock_write();
            for sb in sbs.iter_mut() {
                state.dev_attach_bdev(sb, out)?;
            }
        }

        if self.opts().no_version_check.get() == 0 {
            /*
             * Do this early, so that we never expose a filesystem object that
             * hasn't been version downgraded
             */
            self.opt_version_init(out)?;
        }

        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            prt!(out, "*** DEBUG BUILD ***\n");
        }

        /* first: a UUID already open fails as that, not as a sysfs name taken */
        register(self)?;
        self.online()
    }

    #[inline(never)]
    fn opt_version_init(&self, out: &mut Printbuf) -> Result<(), BchError> {
        use self::FsFlag as Flag;
        let opts = self.opts();

        {
            let mut sb = self.sb_lock();
            let w = self.opts_writer(&mut sb);

            if opts.norecovery.get() != 0 {
                let snapshots_read = RecoveryPass::snapshots_read as _;
                w.recovery_pass_last.set(match opts.recovery_pass_last.get() {
                    0    => snapshots_read,
                    last => last.min(snapshots_read),
                });
                w.nochanges.set(1);
            }

            if opts.nochanges.get() != 0 {
                w.read_only.set(1);
            }

            if opts.will_not_start.get() != 0 {
                w.nostart.set(1);
            }

            if opts.journal_rewind.get() != 0 {
                w.fsck.set(1);
            }
        }

        if !(self.feature(SbFeature::small_image) || self.feature(SbFeature::no_default_sb)) ||
           self.will_resize_on_mount() {
            self.flags.set(Flag::may_upgrade_downgrade, true);
        }

        prt!(out, "starting version {}\n", self.sb.version.get());

        let mut mount_opts = Printbuf::new();
        crate::opts::opts_to_text(&mut mount_opts, &opts.load(), Some(self), Some(self.sb_lock().sb().as_unvalidated()),
                                  None, OptFlags::mount.bits(), 0, OPT_SHOW_MOUNT_STYLE);
        if !mount_opts.as_bytes().is_empty() {
            prt!(out, "with options: ");
            out.write_bytes(mount_opts.as_bytes());
            out.newline();
        }

        if self.sb.version_incompat_allowed.get() != self.sb.version.get() {
            prt!(out, "allowing incompatible features up to {}\n", self.sb.version_incompat_allowed.get());
        }

        if opts.verbose.get() != 0 {
            prt!(out, "features: {}\n", self.sb.features.get());
        }

        if self.sb.multi_device.get() {
            prt!(out, "with devices: ");
            for (i, ca) in self.online_members().enumerate() {
                if i > 0 {
                    out.write_bytes(b",");
                }
                out.write_bytes(ca.name().to_bytes());
            }
            out.newline();
        }

        /* cf_encoding log message should be here, but it breaks xfstests - sigh */

        if opts.journal_rewind.get() != 0 {
            prt!(out, "rewinding journal, fsck required\n");
        }

        if opts.will_not_start.get() == 0 {
            prep_for_start(self, out);
        }

        if self.sb.clean.get() != 0 {
            self.flags.set(Flag::clean_recovery, true);
        }
        if opts.fsck.get() != 0 {
            self.flags.set(Flag::in_fsck, true);
        }
        self.flags.set(Flag::in_recovery, true);

        /* this really should be part of our one multi line mount message, but -
         * xfstests... */
        #[cfg(CONFIG_UNICODE)]
        if self.cf_encoding.as_ref().is_some() {
            crate::bch_info!(self, "Using encoding defined by superblock: utf8-{}.{}.{}",
                      BCH_FS_DEFAULT_UTF8_ENCODING >> 16,
                      (BCH_FS_DEFAULT_UTF8_ENCODING >> 8) & 0xff,
                      BCH_FS_DEFAULT_UTF8_ENCODING & 0xff);
        }

        if self.sb_lock().sb().initialized() {
            if !self.feature(SbFeature::new_extent_overwrite) {
                prt!(out, "feature new_extent_overwrite not set, filesystem no longer supported\n");
                return self.throw(Errcode::EINVAL_missing_new_extent_overwrite);
            }
        }

        fs_mi_field_upgrades(self);

        Ok(())
    }

    fn online(&self) -> Result<(), BchError> {
        #[cfg(kernel)]
        self.chardev_init()?;

        #[cfg(CONFIG_DEBUG_FS)]
        crate::debug::debug::debug_init(self);

        let mut name = Printbuf::new();
        if self.sb.multi_device.get() {
            prt!(&mut name, "{}", self.sb.user_uuid());
        } else {
            name.write_bytes(self.name().to_bytes());
        }
        let parent = crate::init::module::BCACHEFS_KOBJ.kobj();
        if name.allocation_failure() || self.kobj.add(Some(parent), name.as_c_str()).is_err() {
            return self.throw(Errcode::sysfs_init_error);
        }

        let _state = self.state_lock_write();
        for ca in self.members() {
            if ca.sysfs_online().is_err() {
                return self.throw(Errcode::sysfs_init_error);
            }
        }
        Ok(())
    }
}

/// Exits the filesystem when dropped: its ref, put after, is the last of
/// those it's opened with. None once exit() has it.
pub struct OpenFs(Option<FsRef>);

impl OpenFs {
    /// Open with @fs: the ref alloc() made, or into_raw()'s, handed back.
    pub(crate) fn from_ref(fs: FsRef) -> Self {
        OpenFs(Some(fs))
    }

    /// @key unwraps the superblock's key, instead of the keyring
    pub fn open<'a>(devs: impl ExactSizeIterator<Item = &'a CStr>, opts: &mut Opts,
                    key: Option<&Key>) -> Result<OpenFs, BchError> {
        Self::claim(devs, opts, &Holder::new()?)?.open(opts, key)
    }

    /// The open's first half: @devs claimed for @holder, and read
    pub fn claim<'a>(devs: impl ExactSizeIterator<Item = &'a CStr>, opts: &mut Opts,
                     holder: &Arc<Holder>) -> Result<Claimed, BchError> {
        let mut out = Printbuf::new();
        out.indent_add_nextline(2);

        match Self::claim_to(devs, opts, holder, &mut out) {
            Ok(sbs) => Ok(Claimed { sbs, holder: holder.clone(), out }),
            Err(e) => Err(open_failed(e, &mut out)),
        }
    }

    fn claim_to<'a>(devs: impl ExactSizeIterator<Item = &'a CStr>, opts: &mut Opts,
                    holder: &Arc<Holder>, out: &mut Printbuf) -> Result<Vec<SbHandle>, BchError> {
        if devs.len() == 0 {
            return Err(Errcode::EINVAL_open_no_devices.into());
        }

        let mut sbs = Vec::with_capacity(devs.len())?;
        for path in devs {
            sbs.push(read_super(path, opts, Some(holder))?)?;
        }
        filter_dead(&mut sbs, opts, Some(out))?;
        Ok(sbs)
    }

    /// Its ref given up, for C to hold: from_ref() has it back.
    pub fn into_raw(mut self) -> NonNull<Fs> {
        let fs = self.0.take().expect("an OpenFs, exited");
        let p = NonNull::from(&*fs);
        fs.leak();
        p
    }

    /// Drop discards the error
    pub fn exit(mut self) -> i32 {
        ret_to_c(exit(self.0.take().expect("an OpenFs, exited")))
    }
}

fn open_failed(e: BchError, out: &mut Printbuf) -> BchError {
    prt!(out, "error starting filesystem: {}", e.msg());
    print_lines(&LogLevel::ERR.prefix(), out.as_bytes());
    e
}

/// An open's first half: its devices claimed and read, the dead ones dropped
pub struct Claimed {
    sbs:    Vec<SbHandle>,
    holder: Arc<Holder>,
    out:    Printbuf,
}

impl Claimed {
    /// A multi-device filesystem's UUID
    pub fn uuid(&self) -> Option<[u8; 16]> {
        let sb = self.sbs[0].sb();
        sb.multi_device().then_some(sb.uuid.b)
    }

    /// @key unwraps the superblock's key, instead of the keyring
    pub fn open(mut self, opts: &mut Opts, key: Option<&Key>) -> Result<OpenFs, BchError> {
        match self.open_to(opts, key) {
            Err(e) => Err(open_failed(e, &mut self.out)),
            Ok(fs) => {
                if !self.out.as_bytes().is_empty() {
                    LogMsg::with_level(&fs, LogLevel::INFO).write_bytes(self.out.as_bytes());
                }
                Ok(fs)
            }
        }
    }

    fn open_to(&mut self, opts: &mut Opts, key: Option<&Key>) -> Result<OpenFs, BchError> {
        let out = &mut self.out;
        let sbs = &mut self.sbs;

        sbs[0].set_compat_defaults()?;
        let fs_opts = crate::opts::fs_opts(sbs[0].sb().as_unvalidated(), opts, out)?;

        let fs = alloc(sbs, &fs_opts, self.holder.clone(), key, out)?;
        self.holder.fs.store(fs.as_ptr(), core::sync::atomic::Ordering::Release);

        // opt_version_init()'s messages, before startup's
        if !out.as_bytes().is_empty() {
            print_str(&fs, LogLevel::INFO, out);
            out.reset();
        }

        if fs.opts().nostart.get() == 0 {
            fs.start_to(out)?;
        }
        Ok(fs)
    }
}

impl core::ops::Deref for OpenFs {
    type Target = Fs;

    fn deref(&self) -> &Fs {
        self.0.as_deref().expect("an OpenFs, exited")
    }
}

impl Drop for OpenFs {
    fn drop(&mut self) {
        if let Some(fs) = self.0.take() {
            let _ = exit(fs);
        }
    }
}

impl Fs {
    pub fn stop(&self) -> Result<(), BchError> {
        use self::FsFlag as Flag;

        if !self.flags.swap(Flag::stopping, true) {
            self.ro_ref.stop();
            self.ro_ref_events.wake();
            // its pending run holds a ro_ref: put now, not waited out below
            self.counters_recent.cancel_sync();

            if self.flags.get(Flag::started) {
                bch_verbose!(self, "shutting down");
            }

            self.state_lock_write().read_only();

            /* their links reach the block devices by read refs: before they stop */
            for ca in self.members() {
                ca.unlink();
            }

            for ca in self.members() {
                ca.io_ref_stop(READ);
            }

            self.kobj.del();

            #[cfg(CONFIG_DEBUG_FS)]
            crate::debug::debug::debug_exit(self);
            #[cfg(kernel)]
            self.chardev_exit();

            self.ro_ref.wait();

            /* btree prefetch might have kicked off reads in the background: */
            self.btree_flush_all_reads();

            self.btree.interior_updates.work.flush();
        }

        if self.flags.get(Flag::emergency_ro) {
            return self.throw(Errcode::shutdown_with_emergency_ro);
        }
        if self.flags.get(Flag::error) {
            return self.throw(Errcode::shutdown_with_errors_unfixed);
        }
        if self.flags.get(Flag::errors_fixed) {
            return self.throw(Errcode::shutdown_with_errors_fixed);
        }
        Ok(())
    }
}

/// Stopped, its devices freed, and @fs put: the last ref, its release frees
/// it.
fn exit(fs: FsRef) -> Result<(), BchError> {
    let ret = fs.stop();

    crate::init::registry::deregister(&fs);

    fs.journal.writes_in_flight.stop();
    fs.journal.writes_in_flight.wait();

    for i in 0..fs.nr_devices() as usize {
        // freed outside state_lock: dev_free() drains ref_outer, whose holders
        // may be waiting on it
        let ca = fs.state_lock_write().devs()[i].take();
        // stopped: no readers
        if let Some(ca) = ca {
            dev_exit(ca);
        }
    }
    // their drops are a grace period on, and need what the filesystem frees
    crate::util::maybe_rcu_alloc::barrier();

    bch_verbose!(&*fs, "shutdown complete");

    drop(fs);
    ret
}

unsafe impl KobjOwner for Fs {
    const GROUPS: GroupList<FsDir> = FS_GROUPS.list();
    type Dir = FsDir;
    type Alloc = MaybeRcuAlloc<Fs>;

    fn release(mut this: MaybeRcuAlloc<Fs>) {
        teardown(&this);
        // after teardown(): what held &Fs - its threads, its work - is gone
        unsafe { this.btree.placed_drop() };
        let module_ref = this.as_pin_mut().project().module_ref.take();
        drop(this);
        drop(module_ref);
    }
}

fn teardown(fs: &Fs) {
    journal_keys_put_initial(fs);

    fs.reconcile_stop();
    fs.copygc_stop();
    fs.btree_write_buffer_stop();
    fs.btree.async_ops.free_pending();
    free_fsck_errs(fs);
    // the workqueues drained, before the exits free what their work uses
    fs.stop_late();
    reconcile_exit(fs);
    ec_exit(fs);
    fs.tasks().drain();
}

/// Which is the filesystem's superblock: the newest - highest seq, then write
/// time - and of those, the first, as C took it
pub fn newest<'a>(sbs: impl IntoIterator<Item = &'a Sb>) -> Option<usize> {
    sbs.into_iter()
        .map(|sb| core::cmp::Reverse((sb.seq.get(), sb.write_time.get())))
        .enumerate()
        .min_by_key(|&(_, age)| age)
        .map(|(i, _)| i)
}

/// Newest first, without those removed from the filesystem or split from it
pub fn filter_dead(sbs: &mut Vec<SbHandle>, opts: &Opts,
                   mut out: Option<&mut Printbuf>) -> Result<(), BchError> {
    let Some(best) = newest(sbs.iter().map(|h| h.sb())) else {
        return Ok(());
    };

    let mut dead = Vec::from_elem(false, sbs.len())?;
    for i in (0..sbs.len()).rev() {
        let name = sbs[i].io().name().to_str().unwrap_or("?");
        match dev_in_fs(sbs[best].sb(), Some(sbs[best].bdev()), &sbs[i], opts) {
            Ok(()) => {}
            Err(e) if e.matches(Errcode::device_has_been_removed) ||
                      e.matches(Errcode::device_splitbrain) => {
                if let Some(out) = out.as_deref_mut() {
                    prt!(out, "Not using device {name}: {}\n", e.msg());
                }
                dead[i] = true;
            }
            Err(e) => {
                if let Some(out) = out.as_deref_mut() {
                    prt!(out, "Cannot mount with device {name}: {}\n", e.msg());
                }
                return Err(e);
            }
        }
    }

    let best = best - dead[..best].iter().filter(|&&d| d).count();
    let mut i = 0;
    sbs.retain(|_| {
        i += 1;
        !dead[i - 1]
    });
    sbs.swap(0, best);
    Ok(())
}

impl Fs {
    pub fn start(&self) -> Result<(), BchError> {
        let mut err = Printbuf::new();
        log_msg_start(&mut err, self.name());
        let prefix = err.as_bytes().len();

        let ret = self.start_to(&mut err);
        if let Err(e) = &ret {
            prt!(&mut err, "error starting filesystem: {}", e.msg());
        }
        if err.as_bytes().len() != prefix {
            print_str(self, LogLevel::ERR, &mut err);
        }
        ret
    }

    fn start_to(&self, err: &mut Printbuf) -> Result<(), BchError> {
        let ret = self.bring_up(err);
        self.recovery_task.store(core::ptr::null_mut(), Relaxed);
        ret
    }

    fn bring_up(&self, err: &mut Printbuf) -> Result<(), BchError> {
        use self::FsFlag as Flag;
        assert!(!self.flags.get(Flag::started));

        // the open skipped what a start needs: opt_version_init()
        if self.opts().will_not_start.get() != 0 {
            return self.throw(Errcode::EINVAL_will_not_start);
        }
        // superblock writes from here on are part of bringing it up
        self.flags.set(Flag::start_begun, true);

        {
            let state = self.state_lock_write();
            let rcu = read_lock();
            for ca in self.online_devs_rcu(&rcu) {
                if ca.mi.get().state == MemberState::rw {
                    state.dev_allocator_add(ca);
                }
            }
            drop(rcu);
            crate::alloc::capacity::recalc_capacity(self);
        }

        self.may_start(err)?;

        crate::data::reconcile::work::reconcile_init(self)?;
        crate::sb::counters::queue_recent(self);

        // now, so it's persisted before user-data accounting fires: a fresh
        // filesystem shut down before then would never record it, and its next
        // mount would find replicas missing from the superblock
        let _ = self.request_incompat_feature(MetadataVersion::no_sb_user_data_replicas);

        // allocated now if it may be needed: a mount failing on
        // kthread_create() is very annoying
        if crate::init::passes::go_rw_in_recovery(self) {
            self.state_lock_write().init_rw()?;
        }

        // some options can only be checked once started
        crate::opts::opts_hooks_pre_set(self)?;

        let initialized = self.sb_lock().sb().initialized();
        if initialized { crate::init::recovery::fs_recovery(self) } else { crate::init::recovery::fs_initialize(self) }?;

        crate::opts::opts_hooks_pre_set(self)?;

        self.flags.set(Flag::started, true);
        self.ro_ref_events.wake();

        let state = self.state_lock_write();
        if self.opts().read_only.get() != 0 {
            state.read_only();
        } else if !self.flags.get(Flag::rw) {
            state.read_write()?;
        }
        Ok(())
    }

    fn may_start(&self, err: &mut Printbuf) -> Result<(), BchError> {
        use crate::format::DegradedAction as Degraded;

        if self.opts().no_version_check.get() != 0 {
            prt!(err, "Cannot start with opts.no_version_check\n");
            return self.throw(Errcode::EINVAL_no_version_check_start);
        }

        let degraded = self.opts().degraded();
        let flags = if degraded == Degraded::very {
            ForceIf::DEGRADED | ForceIf::LOST
        } else if degraded == Degraded::yes {
            ForceIf::DEGRADED
        } else {
            let mut missing = false;
            for ca in self.members() {
                if !ca.is_online() &&
                   (ca.mi.get().state != MemberState::evacuating ||
                    !ca.has_data().is_empty()) {
                    prt!(err, "Cannot mount without device {}\n", ca.dev_idx);
                    err.indent_add(2);
                    dev_to_text_short(err, self.sb_lock().sb(), ca.dev_idx.into());
                    err.indent_sub(2);
                    missing = true;
                }
            }
            if !missing {
                return Ok(());
            }

            // Which refusal this is decides what there is to consent to -
            // mount.bcachefs asks whoever is at the machine - so it's the
            // error code, not prose. degraded=yes starts while every replica
            // set has a readable copy; past that, only degraded=very.
            let online = self.devs_online.load();
            return self.throw(if self.can_read_fs_with_devs(&online, ForceIf::DEGRADED, Some(err)) {
                Errcode::insufficient_devices_data_intact
            } else {
                Errcode::insufficient_devices_data_lost
            });
        };

        let online = self.devs_online.load();
        let rw = self.allocator.rw_devs[0].load();
        if !self.can_read_fs_with_devs(&online, flags, Some(&mut *err)) ||
           (self.opts().read_only.get() == 0 &&
            !can_write_with_devs(self, &rw, WriteCheck::BCH_WRITE_CHECK_start, flags, err)) {
            self.missing_devs_to_text(err);
            return self.throw(Errcode::insufficient_devices_to_start);
        }
        Ok(())
    }

    #[cold]
    pub fn missing_devs_to_text(&self, out: &mut Printbuf) {
        prt!(out, "Missing devices\n");
        for ca in self.members() {
            if !ca.is_online() && !ca.has_data().is_empty() {
                prt!(out, "Device {}\n", ca.dev_idx);
                out.indent_add(2);
                dev_to_text_short(out, self.sb_lock().sb(), ca.dev_idx.into());
                out.indent_sub(2);
            }
        }
    }
}

/// While one is held, the filesystem can't finish going read-only
pub type WriteRef = RefGuard<Placed<PercpuRef, crate::at!(Fs.writes)>>;

impl Fs {
    /// For work holding a write ref: going read-only drains it
    pub fn write_ref_wq(&self) -> &crate::os::workqueue::Queue {
        &self.write_ref_wq
    }

    /// @make's future, given a write ref - the filesystem, while it holds it;
    /// EROFS if going read-only
    pub fn spawn_write<M>(&self, make: M) -> Result<(), BchError>
    where
        M: for<'w> AsyncFnOnce<(&'w WriteRef,), Output = ()> + 'static,
        for<'w> <M as AsyncFnOnce<(&'w WriteRef,)>>::CallOnceFuture: Send,
    {
        let w: WriteRef = self.require(RefGuard::tryget(&self.writes), Errcode::erofs_no_writes_spawn_write)?;
        crate::util::task::spawn(w, |w| (w.write_ref_wq(), w.tasks()), make)
    }
}

/// A ref on the Fs itself: its last put releases it.
pub type FsRef = RefGuard<Placed<Kobject, crate::at!(Fs.kobj)>>;

#[cfg(kernel)]
pub(crate) type RoRef = RefGuard<Placed<AtomicRef<()>, crate::at!(Fs.ro_ref)>>;

pub fn btree_id_is_alloc(id: u32) -> bool {
    matches!(
        Btree::from_raw(id),
        Some(Btree::alloc
            | Btree::backpointers
            | Btree::stripe_backpointers
            | Btree::need_discard
            | Btree::freespace
            | Btree::bucket_gens
            | Btree::lru
            | Btree::accounting
            | Btree::reconcile_work
            | Btree::reconcile_hipri
            | Btree::reconcile_pending
            | Btree::reconcile_scan)
    )
}

/* We can reconstruct these btrees from information in other btrees */
pub fn btree_id_can_reconstruct(id: u32) -> bool {
    btree_id_is_alloc(id) ||
        matches!(
            Btree::from_raw(id),
            Some(Btree::snapshot_trees
                | Btree::deleted_inodes
                | Btree::reconcile_work
                | Btree::reconcile_hipri
                | Btree::reconcile_pending
                | Btree::reconcile_scan
                | Btree::subvolume_children)
        )
}

/*
 * We can reconstruct BTREE_ID_alloc, but reconstucting it from scratch is not
 * so cheap and OOMs on huge filesystems (until we have online
 * check_allocations)
 */
pub fn btree_id_recovers_from_scan(btree: Btree) -> bool {
    btree == Btree::alloc || !btree_id_can_reconstruct(btree.0)
}
