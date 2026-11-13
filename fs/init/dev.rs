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

use crate::fs::Dev;
use crate::c;
use crate::accounting::{self, dev_usage_init};
use crate::btree::iter::{trans_commit_do, CommitFlags};
use crate::sb::io::SbFieldOps;
use crate::alloc::capacity::recalc_capacity;
use crate::alloc::check::{dev_freespace_init, fs_freespace_init};
use crate::alloc::background::dev_remove_alloc;
use crate::alloc::disk_groups::{dev_group_set, disk_path_to_text_sb};
use crate::alloc::replicas::can_write_with_devs;
use crate::btree::iter::UpdateTriggerFlags;
use crate::data::migrate::{dev_data_drop, dev_data_drop_by_backpointers, dev_remove_stripes, DevDrop};
use crate::debug::sysfs::{DevDir, DEV_GROUPS};
use crate::errcode::{bch_errcode, BchError};
use crate::fs::{Fs, StateLockWrite};
use crate::format::{DataType, MemberInitialized, MemberState, MetadataVersion, SbFeature};
use crate::locking::MemallocFlags;
use crate::sb::io::{read_super, SbReadScratch};
use crate::sb::members::{member_exists, member_state_str, mi_field_upgrades, sb_member,
                         sb_member_alloc, DevOuterRef, DevRef, MEMBER_DELETED_UUID};
use crate::util::alloc::{alloc_pinned, free_pinned, kbox_pin_init};
use crate::util::file::read_file;
use crate::util::kernel::{ktime_get_real_seconds, READ, WRITE};
use crate::util::kobject::{GroupList, KobjOwner, Kobject};
use crate::util::log::{log_dev, log_no_fs, LogLevel};
use crate::util::names;
use crate::util::percpu::PerCpu;
use crate::util::printbuf::{prt_bdevname, DateTime, Msg};
use crate::util::rcu::read_lock;
use crate::util::vstructs::Head;
use crate::util::{Nsecs, Printbuf};
use crate::{bch_err_fn, prt};
use core::ffi::{c_int, c_uint, CStr};
use core::pin::Pin;
use core::ptr::NonNull;
use pin_init::pinned_drop;
use core::sync::atomic::Ordering::Relaxed;
use cstruct_macros::{CStruct, bitfield, TypeInfo};
#[cfg(kernel)]
use {crate::fs::RoRef, crate::sb::io::Holder, crate::util::log::{log_msg_start, print_str},
     crate::util::place::Contained, crate::util::refcount::TryRefcount, crate::util::sleep::Sleep,
     core::sync::atomic::Ordering::Acquire};
use crate::types::{RustOnly, bits_to_longs, c_default, rust_only};
use crate::util::bio::{Bio, BioBox};
use crate::data_structures::bitmap::{AtomicBitArray, BitArray};
use crate::util::ffi::{bytes_in, c_char, strscpy};
use crate::util::refcount::{ARef, AlwaysRefCounted, WaitableRef};
use nestify::nest;

/// A superblock in memory, in a buffer that grows: kvmalloc()'d, a power of
/// two, zeroes past the superblock. Each of a device's superblock, the
/// filesystem's copy, and what format builds.
#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_sb_buf {
    pub sb:          *mut Head<c::bch_sb>,
    pub buffer_size: usize,
    /// The section types edited since the last write, by bit: set by
    /// field_mut() and resizing, for the filesystem's to_cpu() to read.
    pub dirty:       u64,
}
c_default!(bch_sb_buf);

/// The filesystem's superblock, c->disk_sb: what its devices' are copied
/// from. Edited in place through its bch_sb_buf; resized only through
/// Fs::sb_resize(), which grows its devices' with it.
#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_fs_sb {
    #[c_anon("struct bch_sb *sb; size_t buffer_size; u64 dirty")]
    buf: c::bch_sb_buf,
}
c_default!(bch_fs_sb);

impl core::ops::Deref for bch_fs_sb {
    type Target = c::bch_sb_buf;

    fn deref(&self) -> &c::bch_sb_buf {
        &self.buf
    }
}

impl core::ops::DerefMut for bch_fs_sb {
    fn deref_mut(&mut self) -> &mut c::bch_sb_buf {
        &mut self.buf
    }
}

pub type SbBio = rust_only!(Option<BioBox<Bio>>);

nest! {
    /// A device's superblock, and what reads and writes it: what changes
    /// while the device is a filesystem's, under its sb_lock.
    #[repr(C)]
    #[derive(CStruct, TypeInfo)]
    pub struct bch_sb_io {
        #[c_anon("struct bch_sb *sb; size_t buffer_size; u64 dirty")]
        pub buf: c::bch_sb_buf,
        /// Its bio, as big as the buffer, if @have_bio: Rust's alone
        pub bio: SbBio,
        #[c_bitfield]
        #>[bitfield(u8, ty = u32)]
        pub bits: pub struct bch_sb_handle_bits {
            have_layout: u1,
            have_bio:    u1,
            __pad:       u6,
        },
        /// The last seq written
        pub seq: u64,
    }
}

impl Default for bch_sb_io {
    fn default() -> Self {
        bch_sb_io { buf: Default::default(), bio: RustOnly::from_value(None), bits: bch_sb_handle_bits::new(), seq: 0 }
    }
}

/// The block device a superblock was read from, open: fixed while the
/// device is a filesystem's.
#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_sb_bdev {
    pub s_bdev_file: *mut c::file,
    pub bdev: *mut c::block_device,
    pub sb_name: *mut c_char,
    /// A ref, the handle's
    #[c("void *holder")]
    pub holder: *const crate::sb::io::Holder,
    pub mode: c::blk_mode_t,
}
c_default!(bch_sb_bdev);

/// A superblock read from a device: bch2_read_super()'s. To C, one struct
/// of both halves' members.
#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_sb_handle {
    #[c_anon]
    pub io: bch_sb_io,
    #[c_anon]
    pub dev: bch_sb_bdev,
}

impl Default for bch_sb_handle {
    fn default() -> Self {
        bch_sb_handle { io: Default::default(), dev: Default::default() }
    }
}

/// A set of devices, by index.
pub type DevsMask = BitArray<{ bits_to_longs(c::BCH_SB_MEMBERS_MAX as usize) }>;
// A mask written under a lock and read without it - a preference, the
// allocator's placement - can be read mid-write, each word as before or after
unsafe impl crate::util::projections::TearSafe for DevsMask {}

/// One changed under readers - c->devs_online, the allocator's rw_devs[] -
/// declared as such where it's a field: still a struct bch_devs_mask to C.
pub type AtomicDevsMask = AtomicBitArray<{ bits_to_longs(c::BCH_SB_MEMBERS_MAX as usize) }>;

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_devs_mask {
    #[c("unsigned long d[BITS_TO_LONGS(BCH_SB_MEMBERS_MAX)]")]
    pub d: DevsMask,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_devs_list {
    pub nr: u8,
    pub data: [u8; c::BCH_BKEY_PTRS_MAX as usize],
}

fn may_add(fs: &Fs, sb: &c::bch_sb) -> Result<(), BchError> {
    let m = sb_member(sb, sb.dev_idx.into()).unwrap_or_default();

    if u64::from(sb.block_size.get()) != fs.block_bytes() >> 9 {
        return fs.throw(bch_errcode::BCH_ERR_mismatched_block_size);
    }

    if u64::from(m.bucket_size()) < fs.sb_lock().sb().btree_node_size() {
        return fs.throw(bch_errcode::BCH_ERR_bucket_size_too_small);
    }

    Ok(())
}

/* Device startup/shutdown: */

impl Dev {
    /// disk_sb whole, as C's struct bch_sb_handle: to attach a superblock, or
    /// close it.
    ///
    /// # Safety
    /// Nothing else refers to it: state_lock is held for write, and the IO
    /// refs are stopped.
    unsafe fn sb_handle(&self) -> &mut c::bch_sb_handle {
        unsafe { &mut *(&raw const self.disk_sb).cast::<c::bch_sb_handle>().cast_mut() }
    }

    pub fn io_ref_stop(&self, rw: usize) {
        if rw == READ {
            self.fs().devs_online.clear(self.dev_idx.into());
        }

        if !self.io_ref[rw].is_zero() {
            self.io_ref[rw].stop();
            self.io_ref[rw].wait();
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
        #[cfg(kernel)]
        {
            if self.kobj.in_sysfs() && !self.disk_sb.bdev.is_null() {
                let b = self.disk_sb.kobj();
                if b.in_sysfs() {
                    b.unlink(c"bcachefs");
                    self.kobj.unlink(c"block");
                }
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
                return Err(c::ENOMEM.into());
            }
            self.kobj.add(Some(&fs.kobj), name.as_c_str())?;
        }

        #[cfg(kernel)]
        {
            if !self.disk_sb.bdev.is_null() {
                let block = self.disk_sb.kobj();

                block.link(&self.kobj, c"bcachefs")?;
                self.kobj.link(block, c"block")?;
            }
        }

        Ok(())
    }
}

unsafe impl KobjOwner for Dev {
    const GROUPS: GroupList = DEV_GROUPS.list();
    type Dir = DevDir;

    unsafe fn release(this: NonNull<Dev>) {
        unsafe { free_pinned(this.as_ptr()) }
    }
}

#[pinned_drop]
impl PinnedDrop for Dev {
    fn drop(self: Pin<&mut Self>) {
        let ca = unsafe { self.get_unchecked_mut() };

        ca.bucket_backpointer_mismatch.free();
        ca.bucket_backpointer_empty.free();

        unsafe { ca.sb_handle().free() };

        ca.io_done.free();
        ca.buckets_free();
    }
}

unsafe impl AlwaysRefCounted for Dev {
    fn inc_ref(&self) {
        self.kobj.get()
    }

    unsafe fn dec_ref(obj: NonNull<Self>) {
        unsafe { Kobject::put((&raw const (*obj.as_ptr()).kobj).cast()) }
    }
}

/// @ca, out of c->devs[] and past readers: its sysfs gone, and freed with its
/// last ref.
pub(crate) fn dev_free(ca: ARef<Dev>) {
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

    crate::warn_on!(!ca.io_ref[WRITE].is_zero());
    crate::warn_on!(!ca.io_ref[READ].is_zero());

    // not in release(): kobject_put() has taken the kobject out of sysfs by
    // then, and the links in it can't be removed
    ca.unlink();

    if ca.kobj.in_sysfs() {
        ca.kobj.del();
    }
}

/// An attached device freed: its reads stopped, its superblock closed.
pub(crate) fn dev_exit(ca: ARef<Dev>) {
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    assert_eq!(ca.ref_.load(Relaxed), 1);
    ca.io_ref_stop(READ);
    unsafe { ca.sb_handle().free() };
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
        crate::util::rcu::synchronize();
        dev_exit(ca);
    }
}

impl StateLockWrite<'_> {
    pub fn dev_offline(&self, ca: &Dev) {
        if ca.io_ref[READ].is_zero() {
            return;
        }

        self.dev_read_only(ca);

        ca.io_ref_stop(READ);

        ca.unlink();

        unsafe {
            ca.sb_handle().free();
            ca.journal.bio_set.unmake();
        }
    }
}

/// A device not yet in c->devs[]: freed if it's dropped, until attach()
/// publishes it.
struct NewDev(Option<ARef<Dev>>);

impl core::ops::Deref for NewDev {
    type Target = Dev;

    fn deref(&self) -> &Dev {
        self.0.as_deref().expect("a NewDev not yet attached")
    }
}

impl Drop for NewDev {
    fn drop(&mut self) {
        if let Some(ca) = self.0.take() {
            dev_free(ca);
        }
    }
}

fn dev_alloc(fs: &Fs, member: &c::bch_member) -> Option<NewDev> {
    let p = alloc_pinned(Dev::init(fs)).ok()?;
    let ca = unsafe { &mut *p };

    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    unsafe { c::init_completion(&mut ca.ref_completion) };

    ca.mi.set(member.to_cpu());
    ca.btree_allocated_bitmap_gc.set(member.btree_allocated_bitmap.get());

    for (e, m) in ca.errors.iter().zip(member.errors.iter()) {
        e.store(m.get() as i64, Relaxed);
    }

    ca.uuid = member.uuid;

    ca.nr_btree_reserve = c::BTREE_NODE_RESERVE.div_ceil(u32::from(ca.mi.get().bucket_size) / fs.btree_sectors());

    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    ca.ref_.store(1, Relaxed);

    let ok = kbox_pin_init(SbReadScratch::new())
            .map(|s| *ca.sb_read_scratch = Some(Pin::into_inner(s))).is_ok() &&
        ca.buckets_alloc().is_ok() &&
        PerCpu::alloc().map(|io_done| ca.io_done = io_done).is_ok();

    // Kobject::new()'s ref
    let ca = NewDev(Some(unsafe { ARef::from_raw(NonNull::new_unchecked(p)) }));
    ok.then_some(ca)
}

impl NewDev {
    /// Into c->devs[@dev_idx], which owns it from here.
    fn attach(mut self, state: &StateLockWrite<'_>, dev_idx: u32) -> ARef<Dev> {
        let ca = self.0.take().expect("a NewDev attached twice");
        {
            let ca = unsafe { &mut *ca.as_ptr() };

            ca.dev_idx = dev_idx as u8;
            ca.self_mask.d.set(dev_idx as usize);

            if ca.name[0] == 0 {
                let mut name = Printbuf::new();
                prt!(&mut name, "dev-{dev_idx}");
                strscpy(&mut ca.name, name.as_bytes());
            }
        }

        state.devs()[dev_idx as usize].publish(ca.clone());

        if ca.sysfs_online().is_err() {
            log_no_fs(LogLevel::WARN, &Msg(|out: &mut Printbuf| prt!(out, "error creating sysfs objects")));
        }
        ca
    }
}

impl Fs {
    pub fn dev_alloc(&self, dev_idx: u32) -> Result<(), BchError> {
        let member = sb_member(self.sb_lock().sb(), dev_idx).unwrap_or_default();

        let ca = dev_alloc(self, &member).ok_or_else(|| self.err(bch_errcode::BCH_ERR_ENOMEM_dev_alloc))?;

        ca.attach(&self.state_lock_write(), dev_idx);
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
fn read_dev_sysfs_file(dev: c::dev_t, file: &str, ret: &mut [c_char]) -> bool {
    let (major, minor) = (dev >> c::MINORBITS, dev & ((1 << c::MINORBITS) - 1));
    let ret = bytes_in(ret);

    for parent in ["", "../"] {
        let mut path = Printbuf::new();
        prt!(&mut path, "/sys/dev/block/{major}:{minor}/{parent}{file}");

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

pub fn read_identity(bdev: &c::block_device, id: &mut c::bch_dev_identity) {
    let mut bdevname = Printbuf::new();
    prt_bdevname(&mut bdevname, Some(bdev));
    strscpy(&mut id.name, bdevname.as_bytes());

    if !read_dev_sysfs_file(bdev.bd_dev, "device/model", &mut id.model) {
        read_dev_sysfs_file(bdev.bd_dev, "loop/backing_file", &mut id.model);
    }
    read_dev_sysfs_file(bdev.bd_dev, "device/serial", &mut id.serial);
}

/// Whether @sb can be opened as a member of the filesystem whose superblock is
/// @fs. A device that diverged from @fs - split brain - is reported.
pub fn dev_in_fs(fs: &c::bch_sb, fs_bdev: Option<&c::block_device>, sb: &c::bch_sb_handle,
                 opts: &c::bch_opts) -> Result<(), BchError> {
    let dev = sb.io.buf.sb();
    if core::ptr::eq(fs, dev) {
        return Ok(());
    }
    if fs.uuid.b != dev.uuid.b {
        return Err(bch_errcode::BCH_ERR_device_not_a_member_of_filesystem.into());
    }
    if !member_exists(fs, dev.dev_idx.into()) {
        return Err(bch_errcode::BCH_ERR_device_has_been_removed.into());
    }
    if fs.block_size != dev.block_size {
        return Err(bch_errcode::BCH_ERR_mismatched_block_size.into());
    }

    let member_seq = MetadataVersion::member_seq.0;
    if u32::from(fs.version.get()) < member_seq || u32::from(dev.version.get()) < member_seq ||
       opts.no_splitbrain_check_or_default() {
        return Ok(());
    }

    let dev_bdev = Some(sb.dev.bdev());
    let seq_from_fs = sb_member(fs, dev.dev_idx.into()).map_or(0, |m| m.seq.get());

    if fs.seq == dev.seq && fs.write_time != dev.write_time {
        log_no_fs(LogLevel::ERR, &Msg(|out: &mut Printbuf| {
            prt!(out, "Split brain detected between ");
            prt_bdevname(out, dev_bdev);
            prt!(out, " and ");
            prt_bdevname(out, fs_bdev);
            prt!(out, ":\nseq={} but write_time different, got\n", dev.seq.get());
            prt_bdevname(out, fs_bdev);
            prt!(out, " {}\n", DateTime(fs.write_time.get() as i64));
            prt_bdevname(out, dev_bdev);
            prt!(out, " {}\nNot using older sb", DateTime(dev.write_time.get() as i64));
        }));
        return Err(bch_errcode::BCH_ERR_device_splitbrain.into());
    }

    if seq_from_fs != 0 && seq_from_fs < dev.seq.get() {
        log_no_fs(LogLevel::ERR, &Msg(|out: &mut Printbuf| {
            prt!(out, "Split brain detected between ");
            prt_bdevname(out, dev_bdev);
            prt!(out, " and ");
            prt_bdevname(out, fs_bdev);
            prt!(out, ":\n");
            prt_bdevname(out, fs_bdev);
            prt!(out, " believes seq of ");
            prt_bdevname(out, dev_bdev);
            prt!(out, " to be {seq_from_fs}, but ");
            prt_bdevname(out, dev_bdev);
            prt!(out, " has {}\nNot using ", dev.seq.get());
            prt_bdevname(out, dev_bdev);
        }));
        return Err(bch_errcode::BCH_ERR_device_splitbrain.into());
    }
    Ok(())
}

fn attach_bdev(state: &StateLockWrite<'_>, ca: &Dev, sb: &mut c::bch_sb_handle, err: &mut Printbuf)
    -> Result<(), BchError>
{
    let fs = ca.fs();
    let mi = ca.mi.get();

    if ca.is_online() {
        prt!(err, "Cannot attach {}: already have device {} online in slot {}\n",
             sb.dev.name().to_str().unwrap_or("?"), ca, sb.sb().dev_idx);
        return fs.throw(bch_errcode::BCH_ERR_device_already_online);
    }

    let capacity = sb.dev.capacity();
    if capacity < u64::from(mi.bucket_size) * mi.nbuckets {
        prt!(err, "Cannot online {}: device too small (capacity {} filesystem size {} nbuckets {})\n",
             sb.dev.name().to_str().unwrap_or("?"), capacity, u64::from(mi.bucket_size) * mi.nbuckets, mi.nbuckets);
        return fs.throw(bch_errcode::BCH_ERR_device_size_too_small);
    }

    assert!(ca.io_ref[READ].is_zero(), "attaching device {ca} with read refs");
    assert!(ca.io_ref[WRITE].is_zero(), "attaching device {ca} with write refs");

    let ca_p = ca.as_ptr();
    crate::journal::buckets::load(&ca, sb.io.buf.sb())?;
    ca.journal_init(state)?;

    let mut name = Printbuf::new();
    prt_bdevname(&mut name, Some(sb.dev.bdev()));

    /* Commit: */
    unsafe {
        strscpy(&mut (*ca_p).name, name.as_bytes());

        ca.sb_layout.set(sb.sb().layout);
        *ca.sb_handle() = core::mem::take(sb);

        (*ca_p).dev = (*ca.disk_sb.bdev).bd_dev;
    }

    ca.io_ref[READ].start();

    Ok(())
}

impl StateLockWrite<'_> {
    pub fn dev_attach_bdev(&self, sb: &mut c::bch_sb_handle, err: &mut Printbuf) -> Result<(), BchError> {
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

        if ca.io_ref[WRITE].is_zero() {
            ca.io_ref[WRITE].start();
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
    pub fn dev_state_allowed(&self, ca: &Dev, new_state: MemberState, flags: u32,
                             err: &mut Printbuf) -> bool {
        let fs = self.fs();
        let rw = MemberState::rw;

        if ca.mi.get().state == rw && new_state != rw {
            let mut new_rw_devs = c::bch_devs_mask { d: fs.allocator.rw_devs[0].load() };
            new_rw_devs.d.clear(ca.dev_idx.into());

            return can_write_with_devs(fs, &new_rw_devs, c::bch_write_check::BCH_WRITE_CHECK_dev_leaving_rw,
                                       flags, err);
        }

        true
    }

    pub fn dev_set_state(&self, ca: &Dev, new_state: MemberState, flags: u32, err: &mut Printbuf)
        -> Result<(), BchError>
    {
        let fs = self.fs();
        let state = ca.mi.get().state;

        let was_rw = state == MemberState::rw;
        let do_reconcile_scan = new_state == MemberState::rw || new_state == MemberState::evacuating;

        let s = if new_state == MemberState::rw {
            c::reconcile_scan::from_arm(c::reconcile_scan_pending {})
        } else {
            c::reconcile_scan::from_arm(c_uint::from(ca.dev_idx))
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
            return fs.throw(bch_errcode::BCH_ERR_device_state_not_allowed);
        }

        if new_state != MemberState::rw {
            self.dev_read_only(ca);
        }

        log_dev(ca, LogLevel::NOTICE, &Msg(|out: &mut Printbuf| prt!(out, "{}", member_state_str(new_state.0))));

        if do_reconcile_scan {
            fs.request_scan(s, false)?;
        }

        {
            let mut sb = fs.sb_lock();
            sb.fs_member_mut(ca.dev_idx.into()).set_state(new_state.0.into());
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
            fs.request_scan(c::reconcile_scan::from_arm(c::reconcile_scan_stripes {}), false)?;
        }

        if do_reconcile_scan {
            fs.request_scan(s, true)?;
        }

        Ok(())
    }

    pub fn dev_may_offline(&self, ca: &Dev, flags: u32, err: &mut Printbuf) -> Result<(), BchError> {
        let fs = self.fs();

        let mut new_devs = c::bch_devs_mask { d: fs.devs_online.load() };
        new_devs.d.clear(ca.dev_idx.into());

        let mut new_rw_devs = c::bch_devs_mask { d: fs.allocator.rw_devs[0].load() };
        new_rw_devs.d.clear(ca.dev_idx.into());

        if !fs.can_read_fs_with_devs(&new_devs, flags, Some(err)) ||
           (fs.opts().read_only.get() == 0 &&
            !can_write_with_devs(fs, &new_rw_devs, c::bch_write_check::BCH_WRITE_CHECK_dev_leaving_rw,
                                 flags, err)) {
            prt!(err, "Cannot offline required disk\n");
            return fs.throw(bch_errcode::BCH_ERR_device_state_not_allowed);
        }

        Ok(())
    }

    fn dev_remove_locked(&self, ca: &Dev, fast_device_removal: bool, flags: u32,
                         err: &mut Printbuf) -> Result<ARef<Dev>, BchError> {
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
                crate::bch_err_fn!(fs, dev_remove_stripes(fs, DevDrop { dev, flags, err: &mut *err }))?;
            }

            fs.btree_interior_updates_flush();

            /* Check if device still has data before blowing away alloc info */
            let usage = ca.usage_read_fast();
            let mut ret = Ok(());
            for i in 0..c::BCH_DATA_NR {
                let t = DataType(i);
                if !t.is_empty() && !t.is_hidden() && usage.buckets[i as usize] != 0 {
                    if ret.is_ok() {
                        prt!(err, "Remove failed: still has data\n");
                        ret = Err(BchError::from(c::EBUSY));
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
                .inspect_err(|e| prt!(err, "bch2_journal_flush_device_pins() error: {}\n", e.msg()))?;

            fs.journal_flush()
                .inspect_err(|e| prt!(err, "bch2_journal_flush() error: {}\n", e.msg()))?;

            /*
             * Disallow reads before we remove alloc info, otherwise we'll get
             * spurious stale pointer errors:
             */
            self.dev_offline(ca);

            crate::bch_err_msg_dev!(ca, dev_remove_alloc(fs, ca), "removing dev alloc info")
                .inspect_err(|e| prt!(err, "bch2_dev_remove_alloc() error: {}\n", e.msg()))?;

            /*
             * dev_remove_alloc issued btree deletes that need to be durable before
             * we drop the sb member entry:
             */
            fs.journal_flush()
                .inspect_err(|e| prt!(err, "bch2_journal_flush() error: {}\n", e.msg()))?;

            fs.replicas_gc_accounted()
                .inspect_err(|e| prt!(err, "bch2_replicas_gc2() error: {}\n", e.msg()))?;
            /*
             * flushing the journal should be sufficient, but it's the write buffer
             * flush that kills superblock replicas entries after they've gone to 0
             * so bch2_dev_has_data() returns the correct value:
             */

            let data = ca.has_data();
            if data != 0 {
                prt!(err, "Remove failed, still has data ({})\n", names::set::<DataType>(data.into()));
                return Err(BchError::from(c::EBUSY));
            }

            let slot = {
                let _sb = fs.sb_lock();
                self.devs()[ca.dev_idx as usize].take().expect("removing a device that's in c->devs[]")
            };

            #[cfg(not(CONFIG_BCACHEFS_DEBUG))]
            {
                ca.ref_.stop();
                ca.ref_.wait();
            }
            #[cfg(CONFIG_BCACHEFS_DEBUG)]
            unsafe {
                let ca_p = ca.as_ptr();
                ca.dying.set(true);
                crate::sb::members::dev_put(ca_p);
                c::wait_for_completion(&raw mut (*ca_p).ref_completion);
            }
            Ok(slot)
        })();

        if r.is_err() {
            /* The device is staying; allow new references to it again: */
            ca.removing.set(false);

            if fs.flags.get(c::bch_fs_flags::rw) &&
               ca.mi.get().state == MemberState::rw &&
               !ca.io_ref[READ].is_zero() {
                self.dev_read_write(ca);
            }
        }
        r
    }
}

impl Fs {
    pub fn dev_set_state(&self, ca: &Dev, new_state: MemberState, flags: u32,
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
            return self.throw(bch_errcode::BCH_ERR_device_has_been_removed);
        }

        state.dev_set_state(ca, new_state, flags, err)
    }

    /* Device add/removal: */

    pub fn dev_remove(&self, ca: DevOuterRef, flags: u32, err: &mut Printbuf) -> Result<(), BchError> {
        let ca_p = ca.as_ptr();
        let dev_idx = ca.dev_idx;
        let fast_device_removal = self.sb.compat.get() & (1 << c::bch_sb_compat::no_stale_ptrs.0) != 0 &&
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

            state.dev_remove_locked(unsafe { &*ca_p }, fast_device_removal, flags, err)?
        };

        /*
         * The device is now unreachable - not in c->devs, ca->ref drained,
         * ca->removing set. Drain ref_outer holders and free outside
         * state_lock: they may be blocked on it (io_error_work), and will
         * bail via ca->removing once they acquire it.
         */
        dev_free(slot);

        /*
         * Free this device's slot in the bch_member array - all pointers to
         * this device must be gone:
         */
        let mut sb = self.sb_lock();
        let m = sb.fs_member_mut(dev_idx.into());

        if fast_device_removal {
            m.uuid = c::__uuid_t { b: MEMBER_DELETED_UUID };
        } else {
            m.uuid = c::__uuid_t { b: [0; 16] };
        }

        self.write_super(&mut sb);
        Ok(())
    }

    fn dev_set_initialized(&self, ca: &Dev, state: MemberInitialized) -> Result<(), BchError> {
        let mut sb = self.sb_lock();
        sb.fs_member_mut(ca.dev_idx.into()).set_initialized(state.0.into());
        self.write_super_flags(&mut sb, c::bch_sb_write_flags::empty())
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
            crate::journal::buckets::dev_journal_alloc(ca, false)?;
            self.dev_set_initialized(ca, I::initialized)?;
        }

        Ok(())
    }

    /* Add new device to running filesystem: */
    pub fn dev_add(&self, path: &CStr, err: &mut Printbuf) -> Result<(), BchError> {
        let mut opts = c::bch_opts::default();
        let mut sb = read_super(path, &mut opts, Some(self.holder()))
            .inspect_err(|e| prt!(err, "error reading superblock: {}\n", e.msg()))?;

        let mut dev_mi = sb_member(sb.sb(), sb.sb().dev_idx.into()).unwrap_or_default();

        let mut label = Printbuf::new();
        if dev_mi.group() != 0 {
            disk_path_to_text_sb(&mut label, sb.sb(), dev_mi.group() as u32 - 1);
            if label.allocation_failure() {
                return Err(c::ENOMEM.into());
            }
        }

        may_add(self, sb.sb())?;

        let ca = dev_alloc(self, &dev_mi).ok_or(BchError::from(c::ENOMEM))?;
        let started = self.flags.get(c::bch_fs_flags::started);
        let s = c::reconcile_scan::from_arm(c::reconcile_scan_pending {});

        bch_err_fn!(self, (|| -> Result<(), BchError> {
            let state = self.state_lock_write();

            attach_bdev(&state, &ca, &mut sb, err)?;

            let identity = ca.identity();

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
                dev_mi.set_initialized(MemberInitialized::pre_dev_usage.0.into());

                *w.fs_member_mut(dev_idx) = dev_mi;

                unsafe { ca.sb_handle() }.sb_mut().dev_idx = dev_idx as u8;
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
                let _ = c::bch_sb_field_members_v2::to_cpu(self, &mut w);

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
            #[cfg(kernel)]
            {
                unsafe { c::invalidate_bdev(ca.disk_sb.bdev) };

                let mut uuid_str = Printbuf::new();
                prt!(&mut uuid_str, "UUID={}", uuid::Uuid::from_bytes(self.sb.uuid.each_ref().map(|b| b.get())));

                let _ = ca.disk_sb.kobj().uevent_change(&[c"CHANGE=uuid", uuid_str.as_c_str()]);
            }
            Ok(())
        })())?;

        if started {
            self.request_scan(s, true)?;
        }
        Ok(())
    }

    /* Hot add existing device to running filesystem: */
    pub fn dev_bring_online(&self, path: &CStr, err: &mut Printbuf) -> Result<(), BchError> {
        let mut opts = c::bch_opts::default();

        let state = self.state_lock_write();

        /*
         * An online that was waiting on state_lock when shutdown started
         * would find its device offline - stop() has put the io refs - and
         * restart them:
         */
        if self.flags.get(c::bch_fs_flags::stopping) {
            prt!(err, "filesystem is shutting down\n");
            return self.throw(bch_errcode::BCH_ERR_ioctl_fs_stopping);
        }

        let mut sb = read_super(path, &mut opts, Some(self.holder()))
            .inspect_err(|e| prt!(err, "error reading superblock: {}\n", e.msg()))?;

        let dev_idx = u32::from(sb.sb().dev_idx);

        dev_in_fs(self.sb_lock().sb(), None, &sb, &self.opts().load())
            .inspect_err(|e| prt!(err, "device not a member of fs: {}\n", e.msg()))?;

        state.dev_attach_bdev(&mut sb, err)?;

        let ca = state.devs()[dev_idx as usize].get().expect("a device just attached");
        let ca: &Dev = &ca;

        ca.mi_field_upgrades();

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
            .inspect_err(|e| prt!(err, "bch2_dev_add_initialize() error: {}\n", e.msg()))?;

        /*
         * Then reconcile: we didn't know where this device's superblocks were
         * until we saw it, so make the allocations match what the sb says.
         * Idempotent - __bch2_trans_mark_metadata_bucket() assigns rather than
         * accumulates - so this is a no-op when the resume above just did it.
         */
        self.trans_mark_dev_sb(ca, UpdateTriggerFlags::TRANSACTIONAL)
            .inspect_err(|e| prt!(err, "bch2_trans_mark_dev_sb() error: {}\n", e.msg()))?;

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
                .inspect_err(|e| prt!(err, "bch2_dev_freespace_init() error: {}\n", e.msg()))?;
        }

        if ca.journal.nr() == 0 {
            crate::journal::buckets::dev_journal_alloc(&ca, false)
                .inspect_err(|e| prt!(err, "bch2_dev_journal_alloc() error: {}\n", e.msg()))?;
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
        self.request_scan(c::reconcile_scan::from_arm(c::reconcile_scan_pending {}), true)
    }

    pub fn dev_offline(&self, ca: &Dev, flags: u32, err: &mut Printbuf) -> Result<(), BchError> {
        let state = self.state_lock_write();

        if ca.removing() {
            return self.throw(bch_errcode::BCH_ERR_device_has_been_removed);
        }

        if !ca.is_online() {
            prt!(err, "Already offline\n");
            return Ok(());
        }

        state.dev_may_offline(ca, flags, err)?;

        state.dev_offline(ca);
        Ok(())
    }

    pub fn dev_resize(&self, ca: &Dev, nbuckets: u64, err: &mut Printbuf) -> Result<(), BchError> {
        let state = self.state_lock_write();

        if ca.removing() {
            return self.throw(bch_errcode::BCH_ERR_device_has_been_removed);
        }

        let old_nbuckets = ca.mi.get().nbuckets;

        if nbuckets < old_nbuckets {
            prt!(err, "Cannot shrink yet\n");
            return self.throw(bch_errcode::BCH_ERR_EINVAL_dev_resize_shrink);
        }

        let wakeup_reconcile_pending = nbuckets > old_nbuckets;
        let s = c::reconcile_scan::from_arm(c::reconcile_scan_pending {});
        if wakeup_reconcile_pending {
            self.request_scan(s, false)?;
        }

        if nbuckets > u64::from(c::BCH_MEMBER_NBUCKETS_MAX) {
            prt!(err, "New device size too big ({} greater than max {})\n", nbuckets, c::BCH_MEMBER_NBUCKETS_MAX);
            return self.throw(bch_errcode::BCH_ERR_device_size_too_big);
        }

        let bucket_size = u64::from(ca.mi.get().bucket_size);
        if ca.is_online() {
            let capacity = ca.disk_sb.capacity();
            if capacity < bucket_size * nbuckets {
                prt!(err, "New size {} larger than device size {}\n", bucket_size * nbuckets, capacity);
                return self.throw(bch_errcode::BCH_ERR_device_size_too_small);
            }
        }

        ca.buckets_resize(&state, nbuckets)
            .inspect_err(|e| prt!(err, "bch2_dev_buckets_resize() error: {}\n", e.msg()))?;

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
            .inspect_err(|e| prt!(err, "bch2_trans_mark_dev_sb() error: {}\n", e.msg()))?;

        if freespace_initialized {
            ca.resize_alloc(old_nbuckets, nbuckets)
                .inspect_err(|e| prt!(err, "__bch2_dev_resize_alloc() error: {}\n", e.msg()))?;

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
            .ok_or(bch_errcode::BCH_ERR_ENOENT_dev_not_found.into())
    }

    /// @devs by name, space separated; one there isn't, by its number.
    #[cold]
    pub fn devs_list_to_text(&self, out: &mut Printbuf, devs: &c::bch_devs_list) {
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

        trans_commit_do(fs, None, CommitFlags::empty(), |t| {
            accounting::add(t, accounting::DevDataType { dev: self.dev_idx, data_type: DataType::free.0 as u8 },
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
    /// what bch2_latency_acct() added for IO over the latency threshold,
    /// decayed by the time since.
    pub fn congested_read(&self, now: u64) -> u32 {
        let mut congested = i64::from(self.congested.load(Relaxed));
        let last = self.congested_last.load(Relaxed);
        if crate::util::kernel::time_after64(now, last) {
            congested -= (now.wrapping_sub(last) >> 12) as i64;
        }
        congested.clamp(0, c::CONGESTED_MAX.into()) as u32
    }

    fn congested_acct(&self, io_latency: u64, now: u64, rw: usize) {
        let latency_capable = self.io_latency[rw].quantiles.at(1);
        /* ideally we'd be taking into account the device's variance here: */
        let latency_threshold = latency_capable << if rw == READ { 2 } else { 3 };
        let latency_over = io_latency.wrapping_sub(latency_threshold) as i64;

        if latency_threshold != 0 && latency_over > 0 {
            if self.congested.load(Relaxed) < c::CONGESTED_MAX as i32 {
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
        use crate::util::kernel::{local_clock, time_after64};
        use crate::util::time_stats::TimeStats;
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

        self.io_latency[rw].update_at(submit_time, now);
    }
}

/// util.h's ewma_add().
#[cfg(not(CONFIG_BCACHEFS_NO_LATENCY_ACCT))]
fn ewma_add(ewma: u64, val: u64, weight: u32) -> u64 {
    ((ewma << weight).wrapping_sub(ewma).wrapping_add(val)) >> weight
}

#[cfg(not(CONFIG_BCACHEFS_NO_LATENCY_ACCT))]
pub fn bch2_latency_acct(ca: &Dev, submit_time: u64, rw: c_int) {
    ca.latency_acct(submit_time, rw as usize)
}

#[cfg(not(CONFIG_BCACHEFS_NO_LATENCY_ACCT))]
impl Dev {
    /// How congested its IO is - its latency, against what's expected.
    #[cold]
    pub fn congested_to_text(&self, out: &mut Printbuf) {
        use crate::util::kernel::{local_clock, READ, WRITE};
        let now = local_clock();
        out.tabstop_push(32);
        write!(out, "current:\t{}%\n", self.congested_read(now) * 100 / c::CONGESTED_MAX);
        write!(out, "raw:\t{}/{}\n", self.congested.load(Relaxed), c::CONGESTED_MAX);
        prt!(out, "last io over threshold:\t{}\n", Nsecs(now.wrapping_sub(self.congested_last.load(Relaxed))));

        // the thresholds are bch2_latency_acct()'s
        for (what, rw, shift) in [("read", READ, 2), ("write", WRITE, 3)] {
            let q = &self.io_latency[rw].quantiles;
            prt!(out, "{what} latency threshold:\t{}\n", Nsecs(q.at(1) << shift));
            prt!(out, "median {what} latency:\t{}\n", Nsecs(q.at(7)));
        }
    }
}

// ── blk_holder_ops ────────────────────────────────────────────────────────

#[cfg(kernel)]
pub(crate) static HOLDER_OPS: c::blk_holder_ops = c::blk_holder_ops {
    mark_dead: Some(bdev_mark_dead),
    sync: Some(bdev_sync),
    ..::pin_init::zeroed()
};

/// # Safety
/// bd_holder_lock is held; this drops it
#[cfg(kernel)]
unsafe fn bdev_get_fs(bdev: *mut c::block_device) -> Option<RoRef> {
    let r = unsafe {
        let holder = (*bdev).bd_holder.cast::<Holder>();
        (*holder).fs.load(Acquire).as_ref().and_then(|fs: &Fs| fs.ro_ref.tryget())
    };
    unsafe { c::mutex_unlock((&raw mut (*bdev).bd_holder_lock).cast()) };

    let r = r?;
    let fs = r.container();
    let started = |fs: &Fs| fs.flags.get(c::bch_fs_flags::started);
    let Ok(()) = fs.ro_ref_events.wait_until(Sleep::UNINTERRUPTIBLE, || {
        (started(fs) || fs.flags.get(c::bch_fs_flags::stopping)).then_some(())
    });
    // stopped without starting: our ref is what bch2_fs_stop() waits for
    started(fs).then_some(r)
}

#[cfg(kernel)]
unsafe extern "C" fn bdev_mark_dead(bdev: *mut c::block_device, surprise: bool) {
    let Some(r) = (unsafe { bdev_get_fs(bdev) }) else { return };
    let fs = r.container();

    // Not necessary, c->ro_ref guards against the filesystem being unmounted -
    // we only take this to avoid a warning in sync_filesystem
    let sb = fs.vfs_super();
    let _umount = sb.map(|sb| sb.umount().read());
    let state = fs.state_lock_write();
    let Some(ca) = fs.members().find(|ca| ca.disk_sb.bdev == bdev) else { return };

    let mut msg = Printbuf::new();
    log_msg_start(&mut msg, ca.name());
    prt!(&mut msg, "offline from block layer\n");

    let dev = state.dev_may_offline(&ca, c::BCH_FORCE_IF_DEGRADED, &mut msg).is_ok();
    if !dev {
        if let Some(sb) = sb {
            unsafe {
                if !surprise {
                    c::sync_filesystem(sb.as_ptr());
                }
                c::shrink_dcache_sb(sb.as_ptr());
                c::evict_inodes(sb.as_ptr());
            }
        }
        let _ = fs.journal_flush();
    }
    let print = dev || fs.emergency_read_only(&mut msg);

    state.dev_offline(&ca);

    if print {
        print_str(&fs, LogLevel::ERR, &mut msg);
    }
}

#[cfg(kernel)]
unsafe extern "C" fn bdev_sync(bdev: *mut c::block_device) {
    let Some(r) = (unsafe { bdev_get_fs(bdev) }) else { return };
    let fs = r.container();

    if let Some(sb) = fs.vfs_super() {
        let _umount = sb.umount().read();
        unsafe { c::sync_filesystem(sb.as_ptr()) };
    }
}

// ── Resizing on mount ────────────────────────────────────────────────────

impl Dev {
    /// The disk it's on, in sectors: get_capacity()
    fn disk_sectors(&self) -> u64 {
        self.disk_sb.capacity()
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

            crate::bch_info_dev!(&ca, "resizing to size {}", new_nbuckets * u64::from(mi.bucket_size));
            crate::bch_err_fn_dev!(&ca, ca.buckets_resize(state, new_nbuckets))?;

            {
                let mut sb = self.sb_lock();
                let m = sb.fs_member_mut(ca.dev_idx.into());
                m.nbuckets = new_nbuckets.into();
                m.set_resize_on_mount(false);

                let s = sb.sb_mut();
                s.features[0] = (s.features[0].get() & !(1 << SbFeature::small_image.0)).into();
                self.write_super(&mut sb);
            }

            if ca.mi.get().freespace_initialized {
                ca.resize_alloc(old_nbuckets, new_nbuckets)?;
            }
        }
        Ok(())
    }
}
