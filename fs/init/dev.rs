// SPDX-License-Identifier: GPL-2.0

/* DOC_LATEX(device-management)
 * bcachefs is a multi-device filesystem: a single filesystem can span any number
 * of block devices, each contributing storage capacity and IO bandwidth. Devices
 * need not be the same size or have the same performance characteristics---the
 * \hyperref[sec:write-points]{allocator} stripes across all available devices,
 * biasing toward devices with more free space so that all devices fill at the
 * same rate, and the read path
 * tracks per-device IO latency to direct reads to the fastest available replica.
 *
 * Devices can be added and removed at any time without unmounting.
 *
 * \subsubsection{Per-device metadata}
 *
 * Each device has a \texttt{bch\_member} entry in the
 * \hyperref[sec:superblock]{superblock} containing:
 *
 * \begin{itemize}
 * \item \textbf{Identity}: per-device UUID, device name, model string
 * \item \textbf{Geometry}: bucket count, bucket size, first usable bucket
 * \item \textbf{State}: rw, ro, evacuating, or spare (see below)
 * \item \textbf{Configuration}: durability, data-type restrictions
 *   (\texttt{data\_allowed}), discard (TRIM) support, rotational hint
 * \item \textbf{Diagnostics}: cumulative error counters (read, write,
 *   checksum), performance measurements (sequential and random IO rates),
 *   last mount timestamp
 * \end{itemize}
 *
 * \subsubsection{Device states}
 *
 * Each device has a persistent state stored in the superblock:
 *
 * \begin{description}
 * \item[\texttt{rw}] Read-write: fully operational, participates in allocation
 * \item[\texttt{ro}] Read-only: can be read from but receives no new writes
 * \item[\texttt{evacuating}] Being emptied of data prior to removal
 * \item[\texttt{spare}] Reserved, not currently participating in IO
 * \end{description}
 *
 * Device state is changed with \texttt{bcachefs device set-state}. Transitions
 * that would reduce write redundancy below the configured replication level
 * require the \texttt{-{}-force} flag.
 *
 * Separately from the persistent state, a device can be \emph{online} (kernel
 * has the device open) or \emph{offline} (device is listed in the superblock but
 * not currently accessible).
 *
 * \subsubsection{Durability}
 *
 * The \texttt{durability} setting controls how many replicas a copy on a given
 * device counts for. The default is 1. Setting \texttt{durability=2} on a
 * hardware RAID device tells bcachefs that data on that device already has
 * internal redundancy---it counts as two replicas, so the filesystem does not
 * need to keep an additional copy elsewhere. Setting \texttt{durability=0} means
 * copies on the device do not count toward replication requirements at all---the
 * device can only be used as a cache.
 *
 * \subsubsection{Caching}
 *
 * When an extent has multiple copies on different devices, some of those copies
 * may be marked as \emph{cached}. Cached copies are evicted in LRU order by the
 * allocator when the device needs space. Caching behavior is controlled through
 * the target options:
 *
 * \begin{description}
 * \item[Writeback caching] Set \texttt{foreground\_target} and
 *   \texttt{promote\_target} to the cache device, and
 *   \texttt{background\_target} to the backing device. Writes land on the fast
 *   device first and migrate to the backing device in the background.
 * \item[Writearound caching] Set \texttt{foreground\_target} to the backing
 *   device and \texttt{promote\_target} to the cache device. Writes go directly
 *   to the backing device; frequently-read data is promoted to the cache.
 * \end{description}
 *
 * The \texttt{durability=0} setting is essential for cache devices: it ensures
 * bcachefs does not count cached copies toward the replica count, so losing the
 * cache device never causes data loss.
 *
 * \subsubsection{Adding and removing devices}
 *
 * \begin{description}
 * \item[\texttt{bcachefs device add}] Adds a new device to a mounted
 *   filesystem. The device is formatted with bcachefs metadata and integrated
 *   immediately---new allocations can land on it right away. A label can be
 *   assigned at add time with \texttt{-l}. Other per-device options
 *   (\texttt{-{}-discard}, \texttt{-{}-durability}) can be set at add time.
 *
 *   The new device must have a block size and bucket size compatible with the
 *   existing filesystem. After the device is added, its UUID is published via
 *   uevent so that \texttt{/dev/disk/by-uuid} symlinks are updated, and the
 *   reconcile subsystem is notified to scan for any work on the new device.
 *
 * \item[\texttt{bcachefs device evacuate}] Migrates all data off a device,
 *   displaying progress as sectors are moved. Uses the reconcile subsystem
 *   internally; the device's state transitions to evacuating during the process.
 *   Requires metadata version $\geq$ \texttt{reconcile} (1.33).
 * \item[\texttt{bcachefs device remove}] Removes a fully evacuated device from
 *   the filesystem and erases its metadata. Force flags allow removal even if
 *   some data (\texttt{-f}) or metadata (\texttt{-F}) would be lost.
 *
 *   Two removal code paths exist: the legacy path walks the btree to find and
 *   relocate all references to the device, while the \texttt{fast\_device\_removal}
 *   path (default on newer metadata versions) uses
 *   \hyperref[sec:backpointers]{backpointers} to efficiently locate all data
 *   on the device without a full btree scan.
 *
 * \item[\texttt{bcachefs device online/offline}] Bring a device back online or
 *   take it offline without removing it. Offline devices retain their superblock
 *   membership and can be brought back later. Bringing a device online includes
 *   a splitbrain check against the running filesystem's sequence numbers;
 *   onlining also triggers a reconcile scan to detect any data that may need
 *   re-replication.
 *
 *   Offlining a device requires that the remaining online devices can still
 *   satisfy both read and write requirements---the kernel checks that at least
 *   one device can serve reads and at least one can accept writes for every
 *   replica group. If offlining would leave the filesystem unable to operate,
 *   the request is rejected unless forced.
 * \end{description}
 *
 * The typical device removal workflow: \texttt{bcachefs device evacuate /dev/sda}
 * (wait for completion, watching progress), then \texttt{bcachefs device remove
 * /dev/sda}.
 *
 * \subsubsection{Block layer hot-remove}
 *
 * When the block layer reports a device as dead (e.g., a USB drive is
 * unplugged, or a disk is removed from a hot-swap bay), bcachefs receives
 * a notification and attempts a graceful response. If the device can be
 * offlined without leaving the filesystem unable to operate, it is taken
 * offline automatically. Otherwise, the filesystem transitions to
 * emergency read-only mode to prevent data corruption from writes that
 * can no longer reach all required replicas.
 *
 * \subsubsection{Data-type restrictions}
 *
 * The \texttt{data\_allowed} member field restricts which data types a device
 * can hold: journal, btree, or user data. This allows dedicating fast devices to
 * metadata while slower devices hold only user data, or restricting a device to
 * journal-only for write-ahead log isolation. Restrictions are set at format
 * time or via \texttt{set-fs-option} and are enforced by the
 * \hyperref[sec:write-points]{allocator}.
 *
 * \subsubsection{Degraded mode}
 *
 * When a device is unavailable (failed, offline, or physically disconnected),
 * the filesystem can continue operating in degraded mode if sufficient
 * redundancy remains. The number of tolerable failures per replica group is
 * \texttt{nr\_devs - nr\_required}: with 3-way replication, one device can fail
 * without data loss.
 *
 * The \texttt{degraded} mount option controls behavior when devices are missing:
 *
 * \begin{description}
 * \item[\texttt{degraded=true}] Allow mounting with missing devices (read-only
 *   access to degraded data)
 * \item[\texttt{degraded=run}] Allow mounting and normal operation with missing
 *   devices
 * \item[\texttt{degraded=very}] Allow mounting even if writes cannot maintain
 *   the requested replica count (\textbf{dangerous}---creates splitbrain risk)
 * \end{description}
 *
 * While degraded, the filesystem has reduced safety margin---further device loss
 * may cause data unavailability. The reconcile subsystem will automatically
 * repair degraded data by re-replicating to available devices.
 *
 * \subsubsection{Resize}
 *
 * \texttt{bcachefs device resize} grows a device to use additional space
 * (shrinking is not yet supported). If no size is specified, the device grows to
 * fill its underlying block device. Resize works online---no unmount required.
 * The new size is subject to a maximum bucket count
 * (\texttt{BCH\_MEMBER\_NBUCKETS\_MAX}); resize will fail if the requested size
 * would exceed this limit. After resize, the reconcile subsystem is notified to
 * account for the newly available space.
 *
 * \texttt{bcachefs device resize-journal} adjusts the per-device journal size
 * independently of the data area.
 *
 * \subsubsection{Device failure and error tracking}
 *
 * Each device tracks cumulative error counters (read, write, checksum) in the
 * superblock members section. These counters persist across mounts and help
 * identify failing hardware before catastrophic failure. The
 * \texttt{write\_error\_timeout} option (default 30 seconds) controls how long
 * sustained write errors must persist before the device is automatically set to
 * read-only.
 *
 * When a device is set to read-only due to errors, reads can still be served
 * from it. If reads also fail, the device should be taken offline entirely to
 * prevent \hyperref[sec:journal]{journal} stalls---the journal cannot reclaim
 * space if it cannot read back btree nodes from a failed device.
 *
 * \subsubsection{Consistency and self-healing}
 *
 * Device membership is tracked in the superblock and cross-validated against
 * on-disk data during recovery. The allocator checks freespace and alloc btrees
 * against each other before using a bucket. Backpointer walks verify that all
 * data on a device is accounted for. If a device is removed or fails, the
 * reconcile subsystem detects under-replicated data and re-replicates it to
 * remaining devices automatically.
 */

//! A bch_dev is allocated as bch_fs is, so its Rust-typed fields have
//! initializers - the bucket bitmaps' Locks - and it's dropped whole when its
//! kobject's last ref is put: after dev_free()'s teardown, which leaves what it
//! freed nulled for those fields' Drops to find.

#![allow(non_camel_case_types)]

use core::ffi::CStr;
use core::pin::Pin;
use core::sync::atomic::Ordering::Relaxed;
use core::sync::atomic::{AtomicI32, AtomicI64, AtomicU64};
use crate::alloc::accounting::{self, dev_usage_init};
use crate::alloc::background::dev_remove_alloc;
use crate::alloc::buckets::{Bucket, BucketGensArray, DevUsageFull};
use crate::alloc::capacity::recalc_capacity;
use crate::alloc::check::{dev_freespace_init, fs_freespace_init};
use crate::alloc::check_backpointers::{BucketBitmap, BucketBitmapWords};
use crate::alloc::discard::{do_discards_fast, do_invalidates_work};
use crate::alloc::disk_groups::{dev_group_set, disk_path_to_text_sb};
use crate::alloc::replicas::{can_write_with_devs, WriteCheck};
use crate::bcachefs_ioctl::ForceIf;
use crate::btree::interior::BTREE_NODE_RESERVE;
use crate::btree::iter::{CommitFlags, UpdateTriggerFlags};
use crate::c;
use crate::data::dev_bio::DevBio;
use crate::data::migrate::{dev_data_drop, dev_data_drop_by_backpointers, dev_remove_stripes,
    DevDrop};
use crate::data::reconcile::work::ReconcileScan;
use crate::data_structures::bitmap::{AtomicBitArray, AtomicBitflags, AtomicULong, BitArray};
use crate::data_structures::dst_room::DstRoom;
use crate::data_structures::genradix::SharedGenRadix;
use crate::debug::sysfs::{DEV_GROUPS, DevDir};
use crate::errcode::{BchError, Errcode};
use crate::format::{__uuid_t, BKEY_PTRS_MAX, DataType, MemberInitialized, MemberState,
    MetadataVersion, Sb, SbCompat, SbFeatures, SbLayout};
use crate::fs::{Fs, FsFlag, StateLockWrite};
use crate::init::error::io_error_work;
use crate::journal::buckets::{self, JournalDevice};
use crate::locking::{Guard as _, Locked, LockedBy, MemallocFlags, Mutex};
use crate::opts::Opts;
use crate::os::bio::{Bio, BioBox, BioSet, IO_ALIGN};
use crate::os::blkdev::{self, BdevFile, BlockDevice};
use crate::os::file::read_file;
use crate::os::kernel::{ktime_get_real_seconds, READ, WRITE};
use crate::os::kobject::{into_ref, GroupList, KobjOwner, Kobject};
use crate::os::percpu::{Counters, PerCpu};
use crate::os::workqueue::ReffedWork;
use crate::posix::dev::dev_t;
use crate::sb::io::{read_super, Holder, SbFieldOps, SbReadScratch, SbWriteFlags};
use crate::sb::members::{self, member_exists, mi_field_upgrades, sb_member, sb_member_alloc,
    DevIdentity, DevIoRef, DevKobjRef, DevName, DevOuterRef, DevReadRefKind, DevRef,
    MEMBER_DELETED_UUID, MemberCpu};
use crate::sb::members_format::{MEMBERS_MAX, Member, MemberErrorType, SbFieldMembersV2};
use crate::types::bits_to_longs;
use crate::util::alloc::{kbox_pin_init, AlignedBox, Box, KBox, Vec};
use crate::util::backref::BackRef;
use crate::util::ffi::{bytes_in, c_char, strscpy};
use crate::util::late::Late;
use crate::util::log::Log;
use crate::util::maybe_rcu_alloc::MaybeRcuAlloc;
use crate::util::place::Placed;
use crate::util::projections::Volatile;
use crate::util::rcu::{self, read_lock, RcuDropWorkMember, RcuHead, RcuWorkHead};
use crate::util::refcount::{AtomicRef, PercpuRef, TryRefcount, WaitableRef};
use crate::util::sharing::Unshared;
use crate::util::time_stats::{TimeStatsArray, TimeStatsQuantiles};
use crate::util::vstructs::Validated;
use crate::util::{DateTime, Nsecs, Printbuf, ToText};
use crate::{message, new_lock, prt};
use cstruct_macros::{bch_enum, Fieldwise, Init, MaybeZeroable, TypeInfo};
use pin_init::pin_data;
#[cfg(kernel)]
use {crate::fs::RoRef, crate::util::sleep::Sleep, core::sync::atomic::Ordering::Acquire};

#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable, Fieldwise)]
#[fieldwise(Counters::wrapping_add(rhs), Counters::clamp_nonnegative)]
pub struct IoCount {
    pub sectors: [[u64; DataType::NR as usize]; 2],
}

#[bch_enum(closed)]
#[repr(u32)]
pub enum DevFlag {
    /// a write completed that the device may still hold in its volatile
    /// cache; journal_write_preflush() clears it when it flushes the
    /// device.
    unflushed_writes,
}

pub const CONGESTED_MAX: u32 = 1024;

#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, TypeInfo, Init, RcuHead)]
#[init_args(fs: &Fs)]
pub struct Dev {
    #[pin]
    #[init(PercpuRef::new())]
    pub ref_: Placed<PercpuRef, crate::at!(Dev.ref_)>,
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
    #[pin]
    #[init(AtomicRef::new(()))]
    pub ref_outer: Placed<AtomicRef<()>, crate::at!(Dev.ref_outer)>,
    #[pin]
    #[init(PercpuRef::new_dead::<()>())]
    pub io_ref_read: Placed<PercpuRef, crate::at!(Dev.io_ref_read)>,
    #[pin]
    #[init(PercpuRef::new_dead::<()>())]
    pub io_ref_write: Placed<PercpuRef, crate::at!(Dev.io_ref_write)>,

    /// Its filesystem, from the start, attached or not: dev_add()
    /// writes its superblock before it has a slot, and what fails before then
    /// reaches the filesystem through it. A device is freed before its
    /// filesystem - dev_free(), in the filesystem's teardown, drains
    /// ref_outer's holders first - so the filesystem outlives every use.
    #[init(unsafe { BackRef::new(fs) })]
    pub fs: BackRef<Fs>,

    pub dev_idx: u8,
    /*
     * Device is being removed and its alloc info and stripe pointers are
     * about to be deleted: new references must not be created. Checked by
     * __ec_stripe_create(), which may hold pre-invalidation copies of
     * stripe pointers; set by bch2_dev_remove() before the data drop,
     * cleared if removal fails.
     */
    pub removing: Volatile<bool>,
    /*
     * Cached version of this device's member info from superblock
     * Committed by bch2_write_super() -> bch_fs_mi_update(): written under
     * sb_lock, read without it
     */
    pub mi: Volatile<MemberCpu>,
    pub btree_allocated_bitmap_gc: Volatile<u64>,
    pub errors: [AtomicI64; MemberErrorType::NR as usize],
    /// Written racing C's plain reads and stores, as C's are: atomic here,
    /// so Rust's are through a shared bch_dev.
    pub write_errors_start: AtomicULong,
    pub flags: AtomicBitflags<DevFlag>,

    pub uuid: __uuid_t,
    pub name: Volatile<DevName>,
    #[pin]
    #[init(unsafe { Kobject::new::<Dev>() })]
    pub(crate) kobj: Placed<Kobject, crate::at!(Dev.kobj)>,

    /// Its superblock, sb_lock's - through Fs::dev_sb() - and the block
    /// device it's on, fixed while it's attached.
    #[init(LockedBy::new(Default::default(), ::pin_init::zeroed()))]
    pub disk_sb: LockedBy<SbIo, SbBdev, crate::at!(Fs.sb)>,
    /// Its superblock's layout, for what can't take sb_lock to read disk_sb -
    /// whose buffer write_super can reallocate: the allocator, steering clear
    /// of superblock buckets. Copied as it's attached, and as it's edited -
    /// DevSbMut::edit_layout(), under sb_lock.
    pub sb_layout: Volatile<SbLayout>,
    #[pin]
    #[init(None)]
    pub(crate) sb_read_scratch: Option<KBox<SbReadScratch>>,
    pub dev: Volatile<dev_t>,

    pub self_mask: DevsMask,

    /*
     * Buckets:
     * Per-bucket arrays are protected by either rcu_read_lock or
     * state_lock, for device resize.
     */
    pub buckets_gc: SharedGenRadix<Bucket>,
    pub bucket_gens: LockedBy<(), BucketGensArray, crate::at!(Fs.state_lock)>,
    /* allocated once, by migrate, and read without a lock */
    pub buckets_nouse: LockedBy<(), BucketBitmapWords, crate::at!(Fs.state_lock)>,

    #[pin]
    #[init(BucketBitmap::init())]
    pub bucket_backpointer_mismatch: BucketBitmap,
    #[pin]
    #[init(BucketBitmap::init())]
    pub bucket_backpointer_empty: BucketBitmap,

    pub usage: PerCpu<DevUsageFull>,

    /* Allocator: */
    /* where each btree_bitmap kind of allocation scans from: racy, as in C */
    pub alloc_cursor: [Volatile<u64>; 3],

    /*
     * Incremented by alloc_wake() at every site that wakes
     * freelist_events for a specific device. Waiters on
     * c->allocator.freelist_events snapshot the counters for the devices
     * they need at park time and compare on wake: if none have advanced,
     * the wake didn't concern this waiter and it re-parks without the
     * full alloc retry. Fs-wide wakes bump allocator.wake_all_counter
     * instead (see alloc_wake_all()).
     */
    pub alloc_wake_counter: AtomicI32,

    /* Buckets copygc has queued for evacuation on this device: */
    pub copygc_in_flight: AtomicI32,

    /* open buckets still free: under freelist_lock, read without it */
    pub nr_open_buckets: Volatile<u32>,
    /* on the partial list: written under freelist_lock */
    pub nr_partial_buckets: Volatile<u32>,
    pub nr_btree_reserve: u32,

    /* this device's share of c->capacity.reserved, which is their sum:
     * recalc_capacity()'s, under state_lock, read without it */
    pub reserved_sectors: Volatile<u64>,

    #[pin]
    pub(crate) invalidate_work:
        ReffedWork<do_invalidates_work, (crate::field!(io_ref_write), crate::field!(fs->writes))>,

    #[pin]
    pub(crate) discard_fast_work:
        ReffedWork<do_discards_fast, (crate::field!(io_ref_write), crate::field!(fs->writes))>,
    /// Buckets queued for fast discard: alloc/discard.rs's alone.
    #[pin]
    #[init(new_lock!(Vec::new()))]
    pub(crate) discard_fast: Locked<Mutex, Vec<u64>>,
    /// Discards in flight on it, by every worker: a futex word, a worker at
    /// the limit sleeping on it.
    #[pin]
    #[init(core::sync::atomic::AtomicU8::new(0))]
    pub(crate) discards_in_flight: core::sync::atomic::AtomicU8,
    /// c->devs[]'s RCU readers may still have it, and take its io refs, which
    /// its drop exits: dropped a grace period on, in process context.
    #[pin]
    #[rcu_head]
    #[init(RcuDropWorkMember::default())]
    pub(crate) _rcu: RcuDropWorkMember<RcuWorkHead>,

    pub rebalance_work: AtomicI64,

    #[pin]
    pub journal: JournalDevice,
    pub prev_journal_sector: Volatile<u64>,

    /// The bios of writes' replicas on it: write_replicas.rs's
    pub(crate) write_clones: Late<BioSet<DevBio>>,

    #[pin]
    pub(crate) io_error_work: ReffedWork<io_error_work, crate::field!(ref_outer)>,

    /* The rest of this all shows up in sysfs */
    pub cur_latency: [AtomicI64; 2],
    #[pin]
    #[init(TimeStatsArray::new())]
    pub io_latency: TimeStatsArray<TimeStatsQuantiles, 2>,

    pub congested: AtomicI32,
    /* written racily, by bch2_latency_acct() */
    pub congested_last: AtomicU64,

    pub io_done: PerCpu<IoCount>,
}

/// What a superblock's read into, and kept in: IO-aligned, as it's read and
/// written in place.
pub(crate) type SbStorage = AlignedBox<[u64], IO_ALIGN>;
pub(crate) type SbRoom = DstRoom<Sb, SbStorage>;

/// A superblock in memory, in a buffer that grows: a power of two, zeroes past
/// the superblock, page aligned - it's read and written in place, O_DIRECT in
/// userspace. Each of a device's superblock, the filesystem's copy, and what
/// format builds.
#[repr(C)]
#[derive(Default, MaybeZeroable, TypeInfo)]
pub struct bch_sb_buf {
    pub(crate) data: Option<SbRoom>,
    /// The section types edited since the last write, by bit: set by
    /// field_mut() and resizing, for the filesystem's to_cpu() to read.
    pub dirty:       u64,
}

/// The filesystem's superblock, c->disk_sb: what its devices' are copied
/// from. Edited in place through its bch_sb_buf; resized only through
/// Fs::sb_resize(), which grows its devices' with it.
#[repr(C)]
#[derive(Default, MaybeZeroable, TypeInfo)]
pub struct FsSb {
    buf: bch_sb_buf,
}

impl core::ops::Deref for FsSb {
    type Target = bch_sb_buf;

    fn deref(&self) -> &bch_sb_buf {
        &self.buf
    }
}

impl core::ops::DerefMut for FsSb {
    fn deref_mut(&mut self) -> &mut bch_sb_buf {
        &mut self.buf
    }
}

pub type SbBio = Option<BioBox<'static, Bio>>;

/// A device's superblock, what reads and writes it, and the block device it's
/// on as it was opened: what changes while the device is a filesystem's,
/// under its sb_lock. Dropped, the device is closed. With no device - format's,
/// a test's - it's a superblock in memory alone.
#[repr(C)]
#[derive(MaybeZeroable, TypeInfo)]
pub struct SbIo {
    /// Read and validated, or made: never what a read hasn't checked
    pub buf: bch_sb_buf,
    /// Its bio, as big as the buffer, while its device is open: Rust's alone
    pub bio: SbBio,
    /// The last seq written
    pub seq: u64,
    pub s_bdev_file: Option<BdevFile>,
    /// The path it was opened by, NUL-terminated
    pub sb_name: Option<Box<[u8]>>,
    /// A ref, the handle's
    pub holder: *const Holder,
    pub mode: c::blk_mode_t,
}

impl Default for SbIo {
    fn default() -> Self {
        SbIo {
            buf:         Default::default(),
            bio:         None,
            seq:         0,
            s_bdev_file: None,
            sb_name:     None,
            holder:      core::ptr::null(),
            mode:        0,
        }
    }
}

crate::published! {
    /// The block device a superblock was read from, open: fixed while the
    /// device is a filesystem's, and read without sb_lock.
    #[derive(MaybeZeroable)]
    pub struct SbBdev {
        pub bdev: Volatile<*mut BlockDevice>,
    }
}

/// A superblock read from a device and held alone: read_super()'s. A
/// device's is Dev.disk_sb, the same LockedBy, Shared.
pub struct SbHandle(LockedBy<SbIo, SbBdev, (), Unshared>);

impl Default for SbHandle {
    fn default() -> Self {
        SbHandle(LockedBy::new(Default::default(), ::pin_init::zeroed()))
    }
}

impl SbHandle {
    pub fn io(&self) -> &SbIo {
        self.0.get()
    }

    pub fn io_mut(&mut self) -> &mut SbIo {
        self.0.get_mut()
    }

    pub fn dev(&self) -> &SbBdev {
        &self.0
    }

    /// Its block device, which it has open: from read_super() till attaching
    /// it takes it.
    pub fn bdev(&self) -> &BlockDevice {
        self.io().bdev().expect("a superblock handle with no device open")
    }
}

/// A set of devices, by index.
pub type DevsMask = BitArray<{ bits_to_longs(MEMBERS_MAX as usize) }>;

/// One changed under readers - c->devs_online, the allocator's rw_devs[] -
/// declared as such where it's a field.
pub type AtomicDevsMask = AtomicBitArray<{ bits_to_longs(MEMBERS_MAX as usize) }>;

#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable, TypeInfo)]
pub struct DevsList {
    pub nr: u8,
    pub data: [u8; BKEY_PTRS_MAX as usize],
}

fn may_add(fs: &Fs, sb: &Sb) -> Result<(), BchError> {
    let m = sb_member(sb, sb.dev_idx.into()).unwrap_or_else(::pin_init::zeroed);

    if u64::from(sb.block_size.get()) != fs.block_bytes() >> 9 {
        return fs.throw(Errcode::mismatched_block_size);
    }

    if m.bucket_size.get() < fs.sb_lock().sb().btree_node_size() {
        return fs.throw(Errcode::bucket_size_too_small);
    }

    Ok(())
}

/* Device startup/shutdown: */

impl Dev {
    /// With its IO refs stopped: what dereferences the block device holds one.
    /// What reads it without either only tests it, and sees it unpublished
    /// before it's closed.
    fn sb_close(&self) {
        let io = {
            let mut sb = self.fs().sb_lock();
            sb.writer_of(&self.disk_sb).bdev.set(core::ptr::null_mut());
            core::mem::take(sb.get_mut_of(&self.disk_sb))
        };
        // closed outside sb_lock: closing takes the disk's open_mutex, a loop
        // device's open takes lo_mutex under it, losetup holds lo_mutex across
        // fsync of a file here, fsync takes pin_resize_lock, and a journal
        // write holds that across taking sb_lock
        drop(io);
    }

    pub fn io_ref_stop(&self, rw: usize) {
        if rw == READ {
            self.fs().devs_online.clear(self.dev_idx.into());
        }
        self.io_ref_drain(rw);
    }

    fn io_ref_drain(&self, rw: usize) {
        if !self.io_ref(rw).is_zero() {
            self.io_ref(rw).stop();
            self.io_ref(rw).wait();
        }
    }

    pub fn unlink(&self) {
        /*
         * This is racy w.r.t. the underlying block device being hot-removed,
         * which removes it from sysfs.
         *
         * It'd be lovely if we had a way to handle this race, but the sysfs
         * code doesn't appear to provide a good method and block/holder.c is
         * susceptible as well:
         */
        if !self.kobj.in_sysfs() {
            return;
        }
        if let Some(ca) = self.io_ref_read.tryget() {
            let b = blkdev::kobj(ca.bdev());
            if b.in_sysfs() {
                b.unlink(c"bcachefs");
                self.kobj.unlink(c"block");
            }
        }
    }

    pub fn sysfs_online(&self) -> Result<(), BchError> {
        let fs = self.fs();
        if !fs.kobj.in_sysfs() {
            return Ok(());
        }

        if !self.kobj.in_sysfs() {
            let mut name = Printbuf::new();
            prt!(&mut name, "dev-{}", self.dev_idx);
            if name.allocation_failure() {
                return Err(Errcode::ENOMEM_dev_sysfs_name.into());
            }
            self.kobj.add(Some(&fs.kobj), name.as_c_str())?;
        }

        if let Some(ca) = self.io_ref_read.tryget() {
            let block = blkdev::kobj(ca.bdev());

            block.link(&self.kobj, c"bcachefs")?;
            self.kobj.link(block, c"block")?;
        }

        Ok(())
    }
}

unsafe impl KobjOwner for Dev {
    const GROUPS: GroupList<DevDir> = DEV_GROUPS.list();
    type Dir = DevDir;
    type Alloc = MaybeRcuAlloc<Dev>;
}

/// @ca, out of c->devs[] and past readers: its sysfs gone, and freed with its
/// last ref.
pub(crate) fn dev_free(ca: DevKobjRef) {
    /*
     * ref_outer holders (io_error_work, ioctl lookups) may block on
     * state_lock before noticing the device is going away - so we must
     * not drain them while holding it:
     */

    /*
     * io_error_work is never cancelled - it owns a ref_outer per queued
     * instance, so this drain waits for it to run and put:
     */
    ca.ref_outer.stop();
    ca.ref_outer.wait();

    crate::warn_on!(!ca.io_ref_write.is_zero());
    crate::warn_on!(!ca.io_ref_read.is_zero());

    // not in release(): kobject_put() has taken the kobject out of sysfs by
    // then, and the links in it can't be removed
    ca.unlink();

    if ca.kobj.in_sysfs() {
        ca.kobj.del();
    }
}

/// An attached device freed: its IO stopped, its superblock closed.
pub(crate) fn dev_exit(ca: DevKobjRef) {
    ca.io_ref_stop(WRITE);
    ca.io_ref_stop(READ);
    ca.sb_close();
    dev_free(ca);
}

impl Fs {
    /// Device @dev_idx gone from the filesystem in memory, not from its
    /// superblock - that's the caller's: out of c->devs[], so nothing touches
    /// it again, and freed once readers are done with it. For image's
    /// temporary metadata device, which the image is written without.
    pub fn dev_detach(&self, dev_idx: u32) {
        let ca = {
            let state = self.state_lock_write();
            let _sb = self.sb_lock();
            state.devs()[dev_idx as usize].take().expect("detaching a device that's not in c->devs[]")
        };
        rcu::synchronize();
        dev_exit(ca);
    }
}

impl StateLockWrite<'_> {
    pub fn dev_offline(&self, ca: &Dev) {
        if ca.io_ref_read.is_zero() {
            return;
        }

        self.dev_read_only(ca);

        /* its links reach the block device by a read ref: before they stop */
        ca.unlink();

        ca.io_ref_stop(READ);

        ca.sb_close();
    }
}

/// A device not yet in c->devs[]: freed if it's dropped, until attach()
/// publishes it.
struct NewDev(Option<MaybeRcuAlloc<Dev>>);

impl core::ops::Deref for NewDev {
    type Target = Dev;

    fn deref(&self) -> &Dev {
        self.0.as_deref().expect("a NewDev not yet attached")
    }
}

impl Drop for NewDev {
    fn drop(&mut self) {
        if let Some(ca) = self.0.take() {
            let ca = into_ref(ca);
            /* attach_bdev()'s undone - not by io_ref_stop(): its dev_idx
             * isn't its own till attach(), and devs_online's bit is another's */
            ca.io_ref_drain(READ);
            ca.sb_close();
            dev_free(ca);
        }
    }
}

fn dev_alloc(fs: &Fs, member: &Member) -> Option<NewDev> {
    let mut ca = NewDev(Some(MaybeRcuAlloc::new(Dev::init(fs)).ok()?));

    ca.mi.set(member.to_cpu());
    ca.btree_allocated_bitmap_gc.set(member.btree_allocated_bitmap.get());

    for (e, m) in ca.errors.iter().zip(member.errors.iter()) {
        e.store(m.get() as i64, Relaxed);
    }

    {
        let mut this = ca.as_pin_mut().project();
        *this.uuid = member.uuid;
        *this.nr_btree_reserve =
            BTREE_NODE_RESERVE.div_ceil(u32::from(this.mi.get().bucket_size) / fs.btree_sectors());
        this.sb_read_scratch.set(Some(Pin::into_inner(kbox_pin_init(SbReadScratch::new()).ok()?)));
    }
    ca.as_pin_mut().buckets_alloc().ok()?;
    *ca.as_pin_mut().project().io_done = PerCpu::alloc().ok()?;

    Some(ca)
}

impl NewDev {
    fn as_pin_mut(&mut self) -> Pin<&mut Dev> {
        self.0.as_mut().expect("a NewDev not yet attached").as_pin_mut()
    }

    /// Into c->devs[@dev_idx], which owns it from here.
    fn attach(mut self, state: &StateLockWrite<'_>, dev_idx: u32) -> DevKobjRef {
        {
            let this = self.as_pin_mut().project();

            *this.dev_idx = dev_idx as u8;
            this.self_mask.set(dev_idx as usize);

            if this.name.get_mut().is_empty() {
                let mut name = Printbuf::new();
                prt!(&mut name, "dev-{dev_idx}");
                *this.name.get_mut() = DevName::new(name.as_bytes());
            }
        }

        let ca = into_ref(self.0.take().expect("a NewDev attached twice"));
        state.devs()[dev_idx as usize].publish(ca.clone());

        if ca.sysfs_online().is_err() {
            Log::NO_FS.warn(message!("error creating sysfs objects"));
        }
        ca
    }
}

impl Fs {
    pub fn dev_alloc(&self, dev_idx: u32) -> Result<(), BchError> {
        let member = sb_member(self.sb_lock().sb(), dev_idx).unwrap_or_else(::pin_init::zeroed);

        let ca = dev_alloc(self, &member).ok_or_else(|| self.err(Errcode::ENOMEM_dev_alloc))?;

        drop(ca.attach(&self.state_lock_write(), dev_idx));
        Ok(())
    }
}

/*
 * Read a sysfs device file (e.g. "device/model", "device/serial") into @ret,
 * resolving partitions to their parent disk.
 *
 * device/model and device/serial live under the whole disk, not its partitions.
 * Look the device up by dev_t under /sys/dev/block/<maj>:<min>/ (present for
 * both disks and partitions); a partition has no device/ link of its own, so
 * fall back to the parent disk via "..". Mirrors fd_to_dev_model() in the
 * userspace tools (src/wrappers/bdev.rs).
 */
fn read_dev_sysfs_file(dev: dev_t, file: &str, ret: &mut [c_char]) -> bool {
    let ret = bytes_in(ret);

    for parent in ["", "../"] {
        let mut path = Printbuf::new();
        prt!(&mut path, "/sys/dev/block/{dev}/{parent}{file}");

        /* null terminate */
        let nr = read_file(path.as_c_str(), ret).unwrap_or(0).min(ret.len() - 1);
        let nr = ret[..nr].strip_suffix(b"\n").map_or(nr, <[u8]>::len);
        ret[nr] = 0;

        if nr != 0 {
            return true;
        }
    }
    false
}

pub fn read_identity(bdev: &BlockDevice, id: &mut DevIdentity) {
    let mut bdevname = Printbuf::new();
    bdev.to_text(&mut bdevname);
    strscpy(&mut id.name, bdevname.as_bytes());

    let dev = blkdev::dev(bdev);
    if !read_dev_sysfs_file(dev, "device/model", &mut id.model) {
        read_dev_sysfs_file(dev, "loop/backing_file", &mut id.model);
    }
    read_dev_sysfs_file(dev, "device/serial", &mut id.serial);
}

/// Whether @sb can be opened as a member of the filesystem whose superblock is
/// @fs. A device that diverged from @fs - split brain - is reported.
pub fn dev_in_fs(fs: &Sb, fs_bdev: Option<&BlockDevice>, sb: &SbHandle,
                 opts: &Opts) -> Result<(), BchError> {
    let dev = sb.sb();
    if core::ptr::eq(fs, dev) {
        return Ok(());
    }
    if fs.uuid.b != dev.uuid.b {
        return Err(Errcode::device_not_a_member_of_filesystem.into());
    }
    if !member_exists(fs, dev.dev_idx.into()) {
        return Err(Errcode::device_has_been_removed.into());
    }
    if fs.block_size != dev.block_size {
        return Err(Errcode::mismatched_block_size.into());
    }

    let member_seq = MetadataVersion::member_seq.0;
    if u32::from(fs.version.get()) < member_seq || u32::from(dev.version.get()) < member_seq ||
       opts.no_splitbrain_check_or_default() {
        return Ok(());
    }

    let dev_bdev = Some(sb.bdev());
    let seq_from_fs = sb_member(fs, dev.dev_idx.into()).map_or(0, |m| m.seq.get());

    if fs.seq == dev.seq && fs.write_time != dev.write_time {
        Log::NO_FS.err(message!(
            "Split brain detected between {dev_bdev} and {fs_bdev}:\n\
             seq={} but write_time different, got\n\
             {fs_bdev} {}\n\
             {dev_bdev} {}\n\
             Not using older sb",
            dev.seq.get(), DateTime(fs.write_time.get() as i64), DateTime(dev.write_time.get() as i64)));
        return Err(Errcode::device_splitbrain.into());
    }

    if seq_from_fs != 0 && seq_from_fs < dev.seq.get() {
        Log::NO_FS.err(message!(
            "Split brain detected between {dev_bdev} and {fs_bdev}:\n\
             {fs_bdev} believes seq of {dev_bdev} to be {seq_from_fs}, but {dev_bdev} has {}\n\
             Not using {dev_bdev}",
            dev.seq.get()));
        return Err(Errcode::device_splitbrain.into());
    }
    Ok(())
}

fn attach_bdev(state: &StateLockWrite<'_>, ca: &Dev, sb: &mut SbHandle, err: &mut Printbuf)
    -> Result<(), BchError>
{
    let fs = ca.fs();
    let mi = ca.mi.get();

    if ca.is_online() {
        prt!(err, "Cannot attach {}: already have device {} online in slot {}\n",
             sb.io().name().to_str().unwrap_or("?"), ca, sb.sb().dev_idx);
        return fs.throw(Errcode::device_already_online);
    }

    let capacity = blkdev::capacity(sb.bdev());
    if capacity < u64::from(mi.bucket_size) * mi.nbuckets {
        prt!(err, "Cannot online {}: device too small (capacity {} filesystem size {} nbuckets {})\n",
             sb.io().name().to_str().unwrap_or("?"), capacity, u64::from(mi.bucket_size) * mi.nbuckets, mi.nbuckets);
        return fs.throw(Errcode::device_size_too_small);
    }

    assert!(ca.io_ref_read.is_zero(), "attaching device {ca} with read refs");
    assert!(ca.io_ref_write.is_zero(), "attaching device {ca} with write refs");

    buckets::load(&ca, sb.sb())?;
    ca.journal_init(state)?;
    /* a write waits for one only with none held: the reserve's for progress */
    ca.write_clones.init(state, || BioSet::new(4)).map_err(|_| fs.err(Errcode::ENOMEM_dev_write_clones_init))?;

    let mut name = Printbuf::new();
    sb.bdev().to_text(&mut name);
    let dev = blkdev::dev(sb.bdev());

    /* Commit: */
    ca.sb_layout.set(sb.sb().layout);
    {
        let mut fs_sb = fs.sb_lock();
        *fs_sb.get_mut_of(&ca.disk_sb) = core::mem::take(sb.io_mut());
        fs_sb.writer_of(&ca.disk_sb).bdev.set(sb.dev().bdev.get());
        sb.dev().bdev.set(core::ptr::null_mut());
    }

    ca.name.set(DevName::new(name.as_bytes()));
    ca.dev.set(dev);

    ca.io_ref_read.start();

    Ok(())
}

impl StateLockWrite<'_> {
    pub fn dev_attach_bdev(&self, sb: &mut SbHandle, err: &mut Printbuf) -> Result<(), BchError> {
        let fs = self.fs();

        {
            let fs_sb = fs.sb_lock();
            let fs_seq = fs_sb.sb().seq.get();
            if sb.sb().seq.get() > fs_seq {
                /*
                 * rewind, we'll lose some updates but it's not safe to call
                 * bch2_sb_to_fs() after fs is started
                 */
                sb.sb_mut().seq.set(fs_seq);
            }
        }

        let dev_idx = u32::from(sb.sb().dev_idx);
        let ca = self.devs()[dev_idx as usize].get().expect("attaching a device that's not a member");
        let ca: &Dev = &ca;

        attach_bdev(self, ca, sb, err)?;

        fs.devs_online.set(ca.dev_idx.into());

        let _ = ca.sysfs_online();

        fs.reconcile_wakeup();
        Ok(())
    }

    fn dev_read_only(&self, ca: &Dev) {
        let fs = self.fs();

        /*
         * Push journal reclaim so the remaining devices have free journal space
         * before we drop ca from the journal write set below. Otherwise, if ca
         * held the journal's only free space, dropping it strands the journal in
         * journal_full (see journal.flush_dev_ro()). Must come before
         * bch2_dev_allocator_remove(), which pulls ca from rw_devs[journal].
         */
        fs.journal.flush_dev_ro(ca.dev_idx.into());

        /*
         * The allocator thread itself allocates btree nodes, so stop it first:
         */
        fs.dev_allocator_remove(ca);
        recalc_capacity(fs);

        /*
         * bch2_dev_allocator_remove() above blocks until every open_bucket on
         * this device has been released. Submitting write bios and taking
         * write io_refs both happen while we hold open_bucket refs, so that
         * drain also waits for any in-flight submits to complete. Only then
         * is it safe to stop write io_refs: stopping them sooner would cause
         * bch2_dev_get_ioref() to fail at submit time for in-flight writes,
         * surfacing as -EIO to userspace even though the underlying block
         * device is still healthy (we're going read-only, not removing).
         */
        ca.io_ref_stop(WRITE);

        fs.journal.dev_stop(&ca);
        fs.do_discards_async();

        /*
         * may_reuse_stripe() / init_new_stripe_from_old() check
         * bch2_dev_bad_or_evacuating at stripe allocation time, but state can
         * flip to non-rw between that check and the stripe commit. Stripes
         * still on a stripe head were cancelled by stripe_heads_stop() via
         * bch2_dev_allocator_remove() above; stripes that had moved to
         * stripe_new_list (commit in progress) may still commit with a ptr
         * to ca. Wait for them to finish so the subsequent data-drop pass
         * sees their backpointers.
         */
        fs.ec_flush_outstanding();
    }

    fn dev_read_write(&self, ca: &Dev) {
        let fs = self.fs();

        assert_eq!(ca.mi.get().state, MemberState::rw,
                   "device {ca} going read-write, not rw");

        self.dev_allocator_add(ca);
        recalc_capacity(fs);

        if ca.io_ref_write.is_zero() {
            ca.io_ref_write.start();
        }

        fs.do_discards_async();
    }

    /*
     * Note: this function is also used by the error paths - when a particular
     * device sees an error, we call it to determine whether we can just set the
     * device RO, or - if this function returns false - we'll set the whole
     * filesystem RO:
     *
     * XXX: maybe we should be more explicit about whether we're changing state
     * because we got an error or what have you?
     */
    pub fn dev_state_allowed(&self, ca: &Dev, new_state: MemberState, flags: ForceIf,
                             err: &mut Printbuf) -> bool {
        let fs = self.fs();
        let rw = MemberState::rw;

        if ca.mi.get().state == rw && new_state != rw {
            let mut new_rw_devs = fs.allocator.rw_devs[0].load();
            new_rw_devs.clear(ca.dev_idx.into());

            return can_write_with_devs(fs, &new_rw_devs, WriteCheck::BCH_WRITE_CHECK_dev_leaving_rw,
                                       flags, err);
        }

        true
    }

    pub fn dev_set_state(&self, ca: &Dev, new_state: MemberState, flags: ForceIf, err: &mut Printbuf)
        -> Result<(), BchError>
    {
        let fs = self.fs();
        let state = ca.mi.get().state;

        let was_rw = state == MemberState::rw;
        let do_reconcile_scan = new_state == MemberState::rw || new_state == MemberState::evacuating;

        let s = if new_state == MemberState::rw {
            ReconcileScan::pending
        } else {
            ReconcileScan::device(u32::from(ca.dev_idx))
        };

        /* empty stripes hold their buckets until deleted: */
        if new_state == MemberState::evacuating {
            fs.do_stripe_deletes();
        }

        if state == new_state {
            if new_state == MemberState::evacuating {
                return fs.request_scan(s, true);
            }
            return Ok(());
        }

        if !self.dev_state_allowed(ca, new_state, flags, err) {
            return fs.throw(Errcode::device_state_not_allowed);
        }

        if new_state != MemberState::rw {
            self.dev_read_only(ca);
        }

        ca.log().notice(message!("{new_state}"));

        if do_reconcile_scan {
            fs.request_scan(s, false)?;
        }

        {
            let mut sb = fs.sb_lock();
            sb.fs_member_mut(ca.dev_idx.into()).set_state(new_state);
            fs.write_super(&mut sb);
        }

        if new_state == MemberState::rw && ca.is_online() {
            self.dev_read_write(ca);
        }

        /*
         * Any RW transition (in or out) changes the EC widening target (RW
         * members) for this disk_label; queue a stripes scan so can_widen is
         * refreshed. Must come after the SB write above so the scan, which
         * reads ca->mi.state, sees the new state.
         *
         * Going-RW grows the target so newly-widenable stripes can be picked
         * up; going-from-RW shrinks it, and while get_old_stripe demotes
         * lazily on the reuse path, fsck and the canonical can_widen value
         * still need the scan to converge.
         */
        if new_state == MemberState::rw || was_rw {
            fs.request_scan(ReconcileScan::stripes, false)?;
        }

        if do_reconcile_scan {
            fs.request_scan(s, true)?;
        }

        Ok(())
    }

    pub fn dev_may_offline(&self, ca: &Dev, flags: ForceIf, err: &mut Printbuf) -> Result<(), BchError> {
        let fs = self.fs();

        let mut new_devs = fs.devs_online.load();
        new_devs.clear(ca.dev_idx.into());

        let mut new_rw_devs = fs.allocator.rw_devs[0].load();
        new_rw_devs.clear(ca.dev_idx.into());

        if !fs.can_read_fs_with_devs(&new_devs, flags, Some(err)) ||
           (fs.opts().read_only.get() == 0 &&
            !can_write_with_devs(fs, &new_rw_devs, WriteCheck::BCH_WRITE_CHECK_dev_leaving_rw,
                                 flags, err)) {
            prt!(err, "Cannot offline required disk\n");
            return fs.throw(Errcode::device_state_not_allowed);
        }

        Ok(())
    }

    fn dev_remove_locked(&self, ca: &Dev, fast_device_removal: bool, flags: ForceIf,
                         err: &mut Printbuf) -> Result<DevKobjRef, BchError> {
        let fs = self.fs();

        let r = (|| {
            /*
             * Fence stripe creates before we start deleting this device's alloc
             * info and stripe pointers: a stripe create that reused an existing
             * stripe holds pre-invalidation copies of its pointers, and may not
             * commit (or seal, if still accumulating writes) until after our data
             * drop walks have passed it by:
             */
            ca.removing.set(true);

            self.dev_set_state(ca, MemberState::evacuating, flags, err)?;

            /*
             * __bch2_dev_read_only() flushes outstanding stripe creates, but only
             * runs if the device wasn't already evacuating: flush explicitly so
             * creates that passed the ->removing check before we set it complete
             * before the data drop, which needs to see their backpointers:
             */
            fs.ec_flush_outstanding();

            let dev = ca.dev_idx.into();
            if fast_device_removal {
                dev_data_drop_by_backpointers(fs, ca, DevDrop { dev, flags, err: &mut *err })?;
            } else {
                dev_data_drop(fs, DevDrop { dev, flags, err: &mut *err })?;
                fs.log().err_fn(dev_remove_stripes(fs, DevDrop { dev, flags, err: &mut *err }))?;
            }

            fs.btree_interior_updates_flush();

            /* Check if device still has data before blowing away alloc info */
            let usage = ca.usage_read_fast();
            let mut ret = Ok(());
            for i in 0..DataType::NR {
                let t = DataType(i);
                if !t.is_empty() && !t.is_hidden() && usage.buckets[i as usize] != 0 {
                    if ret.is_ok() {
                        prt!(err, "Remove failed: still has data\n");
                        ret = fs.throw(Errcode::EBUSY_dev_remove_has_data);
                    }
                    prt!(err, "  {}: {} buckets\n", t, usage.buckets[i as usize]);
                }
            }
            ret?;

            /*
             * Flush journal pins that reference the device being removed, and any
             * outstanding pins (data_drop's btree updates), before tearing down
             * the device. __bch2_dev_offline below kills ca's io_ref and frees
             * ca->journal.buckets - any journal write that still needs ca in its
             * replicas set will see EROFS.
             */
            fs.journal_flush_outstanding_pins();

            fs.journal.flush_device_pins(ca.dev_idx.into())
                .inspect_err(|e| prt!(err, "journal.flush_device_pins() error: {}\n", e.msg()))?;

            fs.journal_flush()
                .inspect_err(|e| prt!(err, "journal_flush() error: {}\n", e.msg()))?;

            /*
             * Disallow reads before we remove alloc info, otherwise we'll get
             * spurious stale pointer errors:
             */
            self.dev_offline(ca);

            ca.log().err_msg(dev_remove_alloc(fs, ca), crate::message!("removing dev alloc info"))
                .inspect_err(|e| prt!(err, "dev_remove_alloc() error: {}\n", e.msg()))?;

            /*
             * dev_remove_alloc issued btree deletes that need to be durable before
             * we drop the sb member entry - and the replicas entries their
             * accounting left at zero killed, so has_data() is right:
             */
            fs.replicas_settle()
                .inspect_err(|e| prt!(err, "replicas_settle() error: {}\n", e.msg()))?;

            let data = ca.has_data();
            if !data.is_empty() {
                prt!(err, "Remove failed, still has data ({data})\n");
                return fs.throw(Errcode::EBUSY_dev_remove_still_has_data);
            }

            let slot = {
                let _sb = fs.sb_lock();
                self.devs()[ca.dev_idx as usize].take().expect("removing a device that's in c->devs[]")
            };

            ca.ref_.stop();
            ca.ref_.wait();
            Ok(slot)
        })();

        if r.is_err() {
            /* The device is staying; allow new references to it again: */
            ca.removing.set(false);

            if fs.flags.get(FsFlag::rw) &&
               ca.mi.get().state == MemberState::rw &&
               !ca.io_ref_read.is_zero() {
                self.dev_read_write(ca);
            }
        }
        r
    }
}

impl Fs {
    pub fn dev_set_state(&self, ca: &Dev, new_state: MemberState, flags: ForceIf,
                         err: Option<&mut Printbuf>) -> Result<(), BchError>
    {
        let mut no_err = Printbuf::new();
        let err = err.unwrap_or(&mut no_err);

        /*
         * Taking a device out of the allocator shrinks capacity under the
         * reservations dirty pagecache already holds, and the allocator
         * waits for space that isn't coming back: write it out first, while
         * the space is still there.
         *
         * Before state_lock, which writeback may need; trylock because
         * umount holds s_umount while it waits for sysfs writers - us, from
         * the state option - to drain.
         */
        #[cfg(kernel)]
        if ca.mi.get().state == MemberState::rw && new_state != MemberState::rw {
            if let Some(sb) = self.vfs_super() {
                sb.try_sync();
            }
        }

        let state = self.state_lock_write();

        if ca.removing() {
            return self.throw(Errcode::device_has_been_removed);
        }

        state.dev_set_state(ca, new_state, flags, err)
    }

    /* Device add/removal: */

    pub fn dev_remove(&self, ca: DevOuterRef, flags: ForceIf, err: &mut Printbuf) -> Result<(), BchError> {
        let dev_idx = ca.dev_idx;
        let fast_device_removal = self.compat(SbCompat::no_stale_ptrs) &&
            self.request_incompat_feature(MetadataVersion::fast_device_removal).is_ok();

        let slot = {
            let state = self.state_lock_write();

            /*
             * We consume the caller's ref_outer, regardless of whether we succeed
             * or fail - the lookup no longer takes a ca->ref, which we'd deadlock
             * against draining below, and holding ref_outer across bch2_dev_free()
             * would deadlock against the ref_outer drain:
             */
            drop(ca);

            // c->devs[]'s, under state_lock: removed meanwhile, it's not there
            let Some(ca) = state.devs()[dev_idx as usize].get() else {
                return self.throw(Errcode::device_has_been_removed);
            };
            state.dev_remove_locked(&ca, fast_device_removal, flags, err)?
        };

        /*
         * The device is now unreachable - not in c->devs, ca->ref drained,
         * ca->removing set. Drain ref_outer holders and free outside
         * state_lock: they may be blocked on it (io_error_work), and will
         * bail via ca->removing once they acquire it.
         */
        dev_free(slot);

        /*
         * Free this device's slot in the Member array - all pointers to
         * this device must be gone:
         */
        let mut sb = self.sb_lock();
        let m = sb.fs_member_mut(dev_idx.into());

        if fast_device_removal {
            m.uuid = __uuid_t { b: MEMBER_DELETED_UUID };
        } else {
            m.uuid = __uuid_t { b: [0; 16] };
        }

        self.write_super(&mut sb);
        Ok(())
    }

    fn dev_set_initialized(&self, ca: &Dev, state: MemberInitialized) -> Result<(), BchError> {
        let mut sb = self.sb_lock();
        sb.fs_member_mut(ca.dev_idx.into()).set_initialized(state);
        self.write_super_flags(&mut sb, SbWriteFlags::empty())
    }

    pub fn dev_add_initialize(&self, ca: &Dev) -> Result<(), BchError> {
        use MemberInitialized as I;
        let mut state = ca.mi.get().initialized;

        if state == I::pre_dev_usage {
            dev_usage_init(ca, false)?;
            self.dev_set_initialized(ca, I::pre_mark_sb)?;
            state = I::pre_mark_sb;
        }
        if state == I::pre_mark_sb {
            self.trans_mark_dev_sb(ca, UpdateTriggerFlags::TRANSACTIONAL)?;
            self.dev_set_initialized(ca, I::pre_freespace_init)?;
            state = I::pre_freespace_init;
        }
        if state == I::pre_freespace_init {
            fs_freespace_init(self)?;
            self.dev_set_initialized(ca, I::pre_journal_alloc)?;
            state = I::pre_journal_alloc;
        }
        if state == I::pre_journal_alloc {
            buckets::dev_journal_alloc(ca, false)?;
            self.dev_set_initialized(ca, I::initialized)?;
        }

        Ok(())
    }

    /* Add new device to running filesystem: */
    pub fn dev_add(&self, path: &CStr, err: &mut Printbuf) -> Result<(), BchError> {
        let mut opts = Opts::default();
        let mut sb = read_super(path, &mut opts, Some(self.holder()))
            .inspect_err(|e| prt!(err, "error reading superblock: {}\n", e.msg()))?;

        let mut dev_mi = sb_member(sb.sb(), sb.sb().dev_idx.into()).unwrap_or_else(::pin_init::zeroed);

        let mut label = Printbuf::new();
        if dev_mi.group() != 0 {
            disk_path_to_text_sb(&mut label, sb.sb(), dev_mi.group() as u32 - 1);
            if label.allocation_failure() {
                return self.throw(Errcode::ENOMEM_dev_add_label);
            }
        }

        may_add(self, sb.sb())?;

        let ca = self.require(dev_alloc(self, &dev_mi), Errcode::ENOMEM_dev_add_alloc)?;
        let started = self.flags.get(FsFlag::started);
        let s = ReconcileScan::pending;

        self.log().err_fn((|| -> Result<(), BchError> {
            let state = self.state_lock_write();

            let identity = members::identity(sb.bdev());
            attach_bdev(&state, &ca, &mut sb, err)?;

            if started {
                /*
                 * Technically incorrect, but 'bcachefs image update' is the
                 * only thing that adds a device to a not-started filesystem:
                 */
                self.request_scan(s, false)?;
            }

            let ca = {
                let _noio = MemallocFlags::noio();
                let mut w = self.sb_write();
                w.sb_mut().set_multi_device(true);

                self.dev_sb_mut(&mut w, &ca)
                    .expect("a device just attached has its superblock")
                    .sync()
                    .inspect_err(|e| prt!(err, "error setting up new superblock: {}\n", e.msg()))?;

                let dev_idx = sb_member_alloc(self, &mut w)
                    .inspect_err(|e| prt!(err, "error allocating superblock member slot: {}\n", e.msg()))?;

                /* success: */

                dev_mi.last_mount = (ktime_get_real_seconds() as u64).into();
                dev_mi.set_initialized(MemberInitialized::pre_dev_usage);

                *w.fs_member_mut(dev_idx) = dev_mi;

                self.dev_sb_mut(&mut w, &ca)
                    .expect("a device just attached has its superblock")
                    .set_dev_idx(dev_idx as u8);
                let ca = ca.attach(&state, dev_idx);

                self.devs_online.set(dev_idx as usize);

                if dev_mi.group() != 0 {
                    dev_group_set(self, &mut w, dev_idx, label.as_bytes())
                        .inspect_err(|e| prt!(err, "error creating new label: {}\n", e.msg()))?;
                }

                /*
                 * The failure domain string carried over with the
                 * member copy above; re-intern so the new device gets
                 * its failure_domain id.
                 */
                let _ = SbFieldMembersV2::<Validated>::to_cpu(self, &mut w);

                if mi_field_upgrades(&mut w, dev_idx, &identity) {
                    w.dirty();
                }

                /*
                 * We don't call bch2_sb_update() until after the
                 * superblock write finishes - but without sync
                 * c->sb.nr_devices, write_super won't write to the new
                 * device:
                 */
                self.sb.nr_devices.set(w.sb().nr_devices);

                /* We just added a device: the write isn't optional, and
                 * we need its error. On a filesystem not yet started
                 * (bcachefs device add, offline) it's part of bringing
                 * it up. */
                w.bringup();
                w.dirty();
                w.flush()?;
                ca
            };

            if started {
                self.dev_add_initialize(&ca)
                    .inspect_err(|e| prt!(err, "error marking new superblock: {}\n", e.msg()))?;

                if ca.mi.get().state == MemberState::rw {
                    state.dev_read_write(&ca);
                }
            }

            /*
             * We just changed the superblock UUID, invalidate cache and send a
             * uevent to update /dev/disk/by-uuid
             */
            let ca = ca.io_ref_read.tryget().expect("a device just attached, under state_lock");
            blkdev::invalidate(ca.bdev());

            let mut uuid_str = Printbuf::new();
            prt!(&mut uuid_str, "UUID={}", uuid::Uuid::from_bytes(self.sb.uuid.each_ref().map(|b| b.get())));

            let _ = blkdev::kobj(ca.bdev()).uevent_change(&[c"CHANGE=uuid", uuid_str.as_c_str()]);
            Ok(())
        })())?;

        if started {
            self.request_scan(s, true)?;
        }
        Ok(())
    }

    /* Hot add existing device to running filesystem: */
    pub fn dev_bring_online(&self, path: &CStr, err: &mut Printbuf) -> Result<(), BchError> {
        let mut opts = Opts::default();

        let state = self.state_lock_write();

        /*
         * An online that was waiting on state_lock when shutdown started
         * would find its device offline - stop() has put the io refs - and
         * restart them:
         */
        if self.flags.get(FsFlag::stopping) {
            prt!(err, "filesystem is shutting down\n");
            return self.throw(Errcode::ioctl_fs_stopping);
        }

        let mut sb = read_super(path, &mut opts, Some(self.holder()))
            .inspect_err(|e| prt!(err, "error reading superblock: {}\n", e.msg()))?;

        let dev_idx = u32::from(sb.sb().dev_idx);

        dev_in_fs(self.sb_lock().sb(), None, &sb, &self.opts().load())
            .inspect_err(|e| prt!(err, "device not a member of fs: {}\n", e.msg()))?;

        state.dev_attach_bdev(&mut sb, err)?;

        let ca = state.devs()[dev_idx as usize].get().expect("a device just attached");
        let ca: &Dev = &ca;

        if let Some(ca) = ca.io_ref_read.tryget() {
            ca.mi_field_upgrades();
        }

        /*
         * A member can arrive here with its add never finished: the filesystem
         * was initialized or the device added while it was absent, so nothing
         * ever ran bch2_dev_usage_init() for it and its usage counters are
         * zero rather than nbuckets. Resume from wherever it stopped.
         *
         * This has to come before marking the superblock. Marking subtracts the
         * sb buckets from the device's free count, so against a zeroed count it
         * wraps: free becomes -4, the allocator sorts on it, and a device
         * advertising 2^64 free buckets wins every allocation and then blocks
         * in bucket_alloc_blocked forever - taking the journal with it.
         */
        self.dev_add_initialize(ca)
            .inspect_err(|e| prt!(err, "dev_add_initialize() error: {}\n", e.msg()))?;

        /*
         * Then reconcile: we didn't know where this device's superblocks were
         * until we saw it, so make the allocations match what the sb says.
         * Idempotent - __bch2_trans_mark_metadata_bucket() assigns rather than
         * accumulates - so this is a no-op when the resume above just did it.
         */
        self.trans_mark_dev_sb(ca, UpdateTriggerFlags::TRANSACTIONAL)
            .inspect_err(|e| prt!(err, "trans_mark_dev_sb() error: {}\n", e.msg()))?;

        let mi = ca.mi.get();

        if mi.state == MemberState::rw {
            state.dev_read_write(ca);

            /*
             * Re-replicate journal written while this device was gone.
             * After __bch2_dev_read_write(), or the journal write the flush
             * ends with is degraded too; queued because the flush is
             * unbounded and we hold state_lock.
             */
            self.journal.flush_degraded_work.queue(&self.journal.wq);
        }

        if !mi.freespace_initialized {
            dev_freespace_init(self, ca, 0, mi.nbuckets)
                .inspect_err(|e| prt!(err, "dev_freespace_init() error: {}\n", e.msg()))?;
        }

        if ca.journal.nr() == 0 {
            buckets::dev_journal_alloc(&ca, false)
                .inspect_err(|e| prt!(err, "dev_journal_alloc() error: {}\n", e.msg()))?;
        }

        {
            let mut sb = self.sb_lock();
            sb.fs_member_mut(ca.dev_idx.into()).last_mount = (ktime_get_real_seconds() as u64).into();
            self.write_super(&mut sb);
        }

        /*
         * We might have been unable to write because this device was offline:
         *
         * We'd like to limit reconcile pending scans, having them happen
         * because a device is going offline and coming back sucks - but to do
         * that right we need to at least note somewhere /which/ targets have
         * extents on the pending list:
         */
        self.request_scan(ReconcileScan::pending, true)?;
        self.unpark_marks_clear();
        Ok(())
    }

    pub fn dev_offline(&self, ca: &Dev, flags: ForceIf, err: &mut Printbuf) -> Result<(), BchError> {
        let state = self.state_lock_write();

        if ca.removing() {
            return self.throw(Errcode::device_has_been_removed);
        }

        if !ca.is_online() {
            prt!(err, "Already offline\n");
            return Ok(());
        }

        // dev_may_offline() decides by the replicas table: entries gone to
        // zero and not yet killed would refuse. Not if it fails - a failing
        // device is what's offlined, and the table as it is only refuses more
        if self.flags.get(FsFlag::rw) {
            let _ = self.replicas_settle();
        }

        state.dev_may_offline(ca, flags, err)?;

        state.dev_offline(ca);
        Ok(())
    }

    pub fn dev_resize(&self, ca: &Dev, nbuckets: u64, err: &mut Printbuf) -> Result<(), BchError> {
        let state = self.state_lock_write();

        if ca.removing() {
            return self.throw(Errcode::device_has_been_removed);
        }

        let old_nbuckets = ca.mi.get().nbuckets;

        if nbuckets < old_nbuckets {
            prt!(err, "Cannot shrink yet\n");
            return self.throw(Errcode::EINVAL_dev_resize_shrink);
        }

        let wakeup_reconcile_pending = nbuckets > old_nbuckets;
        let s = ReconcileScan::pending;
        if wakeup_reconcile_pending {
            self.request_scan(s, false)?;
        }

        if nbuckets > u64::from(Member::NBUCKETS_MAX) {
            prt!(err, "New device size too big ({} greater than max {})\n", nbuckets, Member::NBUCKETS_MAX);
            return self.throw(Errcode::device_size_too_big);
        }

        let bucket_size = u64::from(ca.mi.get().bucket_size);
        if let Some(ca) = ca.io_ref_read.tryget() {
            let capacity = blkdev::capacity(ca.bdev());
            if capacity < bucket_size * nbuckets {
                prt!(err, "New size {} larger than device size {}\n", bucket_size * nbuckets, capacity);
                return self.throw(Errcode::device_size_too_small);
            }
        }

        ca.buckets_resize(&state, nbuckets)
            .inspect_err(|e| prt!(err, "buckets_resize() error: {}\n", e.msg()))?;

        /*
         * The new range comes into the alloc btree as holes - free buckets - and
         * may hold a backup superblock that was past the old end. It has to be
         * marked before the range goes into freespace, and marking needs
         * nbuckets to cover it (the alloc trigger refuses buckets past the end).
         * So mark with freespace not initialized, as device add does: the alloc
         * trigger doesn't expect the new range in freespace yet, and the
         * allocator steers clear of superblock buckets (is_superblock_bucket()).
         * dev_freespace_init() sets it again; after a crash, mount
         * initializes freespace for the whole device.
         */
        let freespace_initialized = ca.mi.get().freespace_initialized;

        {
            let mut sb = self.sb_lock();
            let m = sb.fs_member_mut(ca.dev_idx.into());
            m.nbuckets = nbuckets.into();
            m.set_freespace_initialized(false);

            self.write_super(&mut sb);
        }

        self.trans_mark_dev_sb(ca, UpdateTriggerFlags::TRANSACTIONAL)
            .inspect_err(|e| prt!(err, "trans_mark_dev_sb() error: {}\n", e.msg()))?;

        if freespace_initialized {
            ca.resize_alloc(old_nbuckets, nbuckets)
                .inspect_err(|e| prt!(err, "resize_alloc() error: {}\n", e.msg()))?;

            self.write_super(&mut self.sb_lock());
        }

        recalc_capacity(self);

        if wakeup_reconcile_pending {
            self.request_scan(s, true)?;
        }
        Ok(())
    }

    /* return with ref on ca->ref: */
    pub fn dev_lookup(&self, name: &CStr) -> Result<DevRef, BchError> {
        let name = name.to_bytes();
        let name = name.strip_prefix(b"/dev/").unwrap_or(name);

        self.members()
            .find(|ca| ca.name().to_bytes() == name)
            .ok_or(Errcode::ENOENT_dev_not_found.into())
    }

    /// @devs by name, space separated; one there isn't, by its number.
    #[cold]
    pub fn devs_list_to_text(&self, out: &mut Printbuf, devs: &DevsList) {
        // room first: under RCU it can only allocate atomically
        out.make_room(1024);
        let rcu = read_lock();

        for (i, &dev) in devs.as_slice().iter().enumerate() {
            if i != 0 {
                write!(out, " ");
            }
            match self.dev_rcu(dev.into(), &rcu) {
                Some(ca) => write!(out, "{ca}"),
                None     => write!(out, "(invalid device {dev})"),
            }
        }
    }
}

/* Resize on mount */

impl Dev {
    pub fn resize_alloc(&self, old_nbuckets: u64, new_nbuckets: u64) -> Result<(), BchError> {
        let fs = self.fs();

        fs.trans_commit_do(None, CommitFlags::empty(), |t| {
            accounting::add(t, accounting::DevDataType { dev: self.dev_idx, data_type: DataType::free.into() },
                            accounting::DevDataTypeCounters {
                                buckets: new_nbuckets.wrapping_sub(old_nbuckets) as i64, sectors: 0, fragmented: 0,
                            }, false)
        })?;
        dev_freespace_init(fs, self, old_nbuckets, new_nbuckets)
    }
}

// ── Congestion ───────────────────────────────────────────────────────────

#[cfg(not(CONFIG_BCACHEFS_NO_LATENCY_ACCT))]
impl Dev {
    /// The device's congestion at @now (local_clock()), out of CONGESTED_MAX:
    /// what latency_acct() added for IO over the latency threshold,
    /// decayed by the time since.
    pub fn congested_read(&self, now: u64) -> u32 {
        let mut congested = i64::from(self.congested.load(Relaxed));
        let last = self.congested_last.load(Relaxed);
        if crate::os::kernel::time_after64(now, last) {
            congested -= (now.wrapping_sub(last) >> 12) as i64;
        }
        congested.clamp(0, CONGESTED_MAX.into()) as u32
    }

    fn congested_acct(&self, io_latency: u64, now: u64, rw: usize) {
        let latency_capable = self.io_latency.get(rw).stats().quantiles.at(1);
        /* ideally we'd be taking into account the device's variance here: */
        let latency_threshold = latency_capable << if rw == READ { 2 } else { 3 };
        let latency_over = io_latency.wrapping_sub(latency_threshold) as i64;

        if latency_threshold != 0 && latency_over > 0 {
            if self.congested.load(Relaxed) < CONGESTED_MAX as i32 {
                self.congested.fetch_add((io_latency.wrapping_mul(2).min(u32::MAX.into()) as u32 /
                                          latency_threshold.min(u32::MAX.into()) as u32) as i32, Relaxed);
            }

            self.congested_last.store(now, Relaxed);
        } else if self.congested.load(Relaxed) > 0 {
            self.congested.fetch_sub(1, Relaxed);
        }
    }

    /// An IO of @rw's, submitted at @submit_time, completed: its latency in
    /// the device's average and quantiles, and - a read's - its congestion.
    pub fn latency_acct(&self, submit_time: u64, rw: usize) {
        use crate::os::kernel::{local_clock, time_after64};
        use core::sync::atomic::Ordering::SeqCst;

        let latency = &self.cur_latency[rw];
        let now = local_clock();
        let io_latency = if time_after64(now, submit_time) { now - submit_time } else { 0 };

        let mut old = latency.load(Relaxed) as u64;
        loop {
            /*
             * If the io latency was reasonably close to the current
             * latency, skip doing the update and atomic operation - most of
             * the time:
             */
            if old.abs_diff(io_latency) < (old >> 1) && now & u64::from(!(!0u32 << 5)) != 0 {
                break;
            }

            let new = ewma_add(old, io_latency, 5);
            match latency.compare_exchange(old as i64, new as i64, SeqCst, Relaxed) {
                Ok(_)  => break,
                Err(v) => old = v as u64,
            }
        }

        /*
         * Only track read latency for congestion accounting: writes are subject
         * to heavy queuing delays from page cache writeback:
         */
        if rw == READ {
            self.congested_acct(io_latency, now, rw);
        }

        self.io_latency.get(rw).update_at(submit_time, now);
    }
}

/// util.h's ewma_add().
#[cfg(not(CONFIG_BCACHEFS_NO_LATENCY_ACCT))]
fn ewma_add(ewma: u64, val: u64, weight: u32) -> u64 {
    ((ewma << weight).wrapping_sub(ewma).wrapping_add(val)) >> weight
}

#[cfg(not(CONFIG_BCACHEFS_NO_LATENCY_ACCT))]
impl Dev {
    /// How congested its IO is - its latency, against what's expected.
    #[cold]
    pub fn congested_to_text(&self, out: &mut Printbuf) {
        use crate::os::kernel::{local_clock, READ, WRITE};
        let now = local_clock();
        out.tabstop_push(32);
        write!(out, "current:\t{}%\n", self.congested_read(now) * 100 / CONGESTED_MAX);
        write!(out, "raw:\t{}/{}\n", self.congested.load(Relaxed), CONGESTED_MAX);
        prt!(out, "last io over threshold:\t{}\n", Nsecs(now.wrapping_sub(self.congested_last.load(Relaxed))));

        // the thresholds are latency_acct()'s
        for (what, rw, shift) in [("read", READ, 2), ("write", WRITE, 3)] {
            let q = &self.io_latency.get(rw).stats().quantiles;
            prt!(out, "{what} latency threshold:\t{}\n", Nsecs(q.at(1) << shift));
            prt!(out, "median {what} latency:\t{}\n", Nsecs(q.at(7)));
        }
    }
}

// ── blk_holder_ops ────────────────────────────────────────────────────────

#[cfg(kernel)]
pub(crate) static HOLDER_OPS: c::blk_holder_ops = blkdev::holder_ops::<Holder>();

/// @r's filesystem, once it's started - or none, if it stopped first: our ref
/// is what the filesystem's stop() waits for.
#[cfg(kernel)]
fn holder_started(r: RoRef) -> Option<RoRef> {
    let fs: &Fs = &r;
    let started = |fs: &Fs| fs.flags.get(FsFlag::started);
    let Ok(()) = fs.ro_ref_events.wait_until(Sleep::UNINTERRUPTIBLE, || {
        (started(fs) || fs.flags.get(FsFlag::stopping)).then_some(())
    });
    started(fs).then_some(r)
}

#[cfg(kernel)]
impl blkdev::HolderOps for Holder {
    type Ref = RoRef;

    fn get(&self) -> Option<RoRef> {
        unsafe { self.fs.load(Acquire).as_ref() }.and_then(|fs: &Fs| fs.ro_ref.tryget())
    }

    fn mark_dead(r: RoRef, bdev: &BlockDevice, surprise: bool) {
        let Some(r) = holder_started(r) else { return };
        let fs: &Fs = &r;

        // Not necessary, c->ro_ref guards against the filesystem being unmounted -
        // we only take this to avoid a warning in sync_filesystem
        let sb = fs.vfs_super();
        let umount = sb.map(|sb| (sb, sb.umount().read()));
        let state = fs.state_lock_write();
        let Some(ca) = fs.members().find(|ca| ca.disk_sb.bdev.get().cast_const() == core::ptr::from_ref(bdev))
        else { return };

        let mut msg = ca.log().msg();
        prt!(&mut msg, "offline from block layer\n");

        let dev = state.dev_may_offline(&ca, ForceIf::DEGRADED, &mut msg).is_ok();
        if !dev {
            if let Some((sb, umount)) = &umount {
                if !surprise {
                    // Ignored, as C ignored it
                    let _ = sb.sync(umount);
                }
                sb.evict_all(umount);
            }
            let _ = fs.journal_flush();
        }
        let print = dev || fs.emergency_read_only(&mut msg);
        msg.set_suppressed(!print);

        state.dev_offline(&ca);
    }

    fn sync(r: RoRef) {
        let Some(r) = holder_started(r) else { return };
        let fs: &Fs = &r;

        if let Some(sb) = fs.vfs_super() {
            // Ignored, as C ignored it
            let _ = sb.sync(&sb.umount().read());
        }
    }
}

// ── Resizing on mount ────────────────────────────────────────────────────

impl DevIoRef<DevReadRefKind> {
    /// The disk it's on, in sectors: get_capacity()
    fn disk_sectors(&self) -> u64 {
        blkdev::capacity(self.bdev())
    }

    fn will_resize_on_mount(&self) -> bool {
        let mi = self.mi.get();
        mi.resize_on_mount && mi.nbuckets < self.disk_sectors() / u64::from(mi.bucket_size)
    }
}

impl Fs {
    pub(crate) fn will_resize_on_mount(&self) -> bool {
        self.online_members().any(|ca| ca.will_resize_on_mount())
    }

    pub fn resize_on_mount(&self, state: &StateLockWrite<'_>) -> Result<(), BchError> {
        for ca in self.online_members() {
            if !ca.will_resize_on_mount() {
                continue;
            }

            let mi = ca.mi.get();
            let old_nbuckets = mi.nbuckets;
            let new_nbuckets = ca.disk_sectors() / u64::from(mi.bucket_size);

            ca.log().info(crate::message!(
                "resizing to size {}", new_nbuckets * u64::from(mi.bucket_size)));
            ca.log().err_fn(ca.buckets_resize(state, new_nbuckets))?;

            {
                let mut sb = self.sb_lock();
                let m = sb.fs_member_mut(ca.dev_idx.into());
                m.nbuckets = new_nbuckets.into();
                m.set_resize_on_mount(false);

                sb.sb_mut().features.update(|f| f - SbFeatures::small_image);
                self.write_super(&mut sb);
            }

            if ca.mi.get().freespace_initialized {
                ca.resize_alloc(old_nbuckets, new_nbuckets)?;
            }
        }
        Ok(())
    }
}
