// SPDX-License-Identifier: GPL-2.0

//! Devices: dev.c's, as it converts.
//!
//! So far devs_list_to_text(), its congestion, its state set through C's
//! dev_set_state(), the block layer's holder ops, and a bch_dev's memory: Rust
//! allocates it, as bch_fs's, so its Rust-typed fields have initializers - the
//! bucket bitmaps' Locks - and it's dropped whole when it's freed: after C's
//! teardown, which leaves what it freed nulled for those fields' Drops to find.

#![allow(non_camel_case_types)]

use crate::fs::Dev;
use crate::c;
use crate::errcode::{bch_errcode, ret_to_c, ret_to_result_void, BchError};
use crate::fs::{Fs, StateLockWrite};
use crate::prt;
use crate::sb::members::{member_exists, sb_member};
use crate::util::log::{log_no_fs, LogLevel};
use crate::util::printbuf::{prt_bdevname, DateTime, Msg};
use crate::util::rcu::read_lock;
use crate::util::vstructs::{Head, Validated};
use crate::util::{Nsecs, Printbuf};
use core::ffi::{c_int, c_uint};
use core::sync::atomic::Ordering::Relaxed;
use cstruct_macros::{c_export, CStruct, bitfield, c_extern, c_typedef, TypeInfo};
#[cfg(kernel)]
use {crate::fs::RoRef, crate::sb::io::Holder, crate::util::log::{log_msg_start, print_str},
     crate::util::place::Contained, crate::util::refcount::TryRefcount, crate::util::sleep::Sleep,
     core::sync::atomic::Ordering::Acquire};
use crate::types::{DArray, bits_to_longs, c_default};
use crate::util::bio::{Bio, BioBufPtr};
use crate::util::bitmap::{AtomicBitArray, BitArray};
use crate::util::ffi::c_char;
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

nest! {
    /// A device's superblock, and what reads and writes it: what changes
    /// while the device is a filesystem's, under its sb_lock.
    #[repr(C)]
    #[derive(CStruct, TypeInfo)]
    pub struct bch_sb_io {
        #[c_anon("struct bch_sb *sb; size_t buffer_size; u64 dirty")]
        pub buf: c::bch_sb_buf,
        /// Its bio, as big as the buffer, if @have_bio: Rust's alone
        #[c("struct bio *bio")]
        pub bio: Option<BioBufPtr<Bio>>,
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
        bch_sb_io { buf: Default::default(), bio: None, bits: bch_sb_handle_bits::new(), seq: 0 }
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

c_typedef! {
    #[c("DARRAY(struct bch_sb_handle) bch_sb_handles")]
    pub type bch_sb_handles = DArray<c::bch_sb_handle>;
}

/// A set of devices, by index.
pub type DevsMask = BitArray<{ bits_to_longs(c::BCH_SB_MEMBERS_MAX as usize) }>;
// A mask written under a lock and read without it - a preference, the
// allocator's placement - can be read mid-write, each word as before or after
crate::tear_safe!(DevsMask);

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

c_extern! {
    pub fn bch2_dev_io_ref_stop(ca: *mut Dev, rw: c_int);
    pub fn bch2_dev_unlink(ca: *mut Dev);
    pub fn bch2_dev_free(ca: *mut Dev);
    pub fn bch2_dev_sysfs_online(c: *mut Fs, ca: *mut Dev) -> c_int;
    pub fn bch2_dev_alloc(c: *mut Fs, dev_idx: c_uint) -> c_int;
    pub fn bch2_dev_attach_bdev(c: *mut Fs, sb: *mut c::bch_sb_handle, err: *mut Printbuf) -> c_int;
    pub fn __bch2_dev_resize_alloc(ca: *mut Dev, old_nbuckets: u64, new_nbuckets: u64) -> c_int;
    pub fn bch2_dev_add(c: *mut Fs, path: *const c_char, err: *mut Printbuf) -> c_int;
    pub fn bch2_dev_add_initialize(c: *mut Fs, ca: *mut Dev) -> c_int;
    pub fn bch2_dev_resize(c: *mut Fs, ca: *mut Dev, nbuckets: u64,
                           err: *mut Printbuf) -> c_int;
    pub fn bch2_dev_remove(c: *mut Fs, ca: *mut Dev, flags: c_int,
                           err: *mut Printbuf) -> c_int;
    pub fn bch2_dev_online(c: *mut Fs, path: *const c_char, err: *mut Printbuf) -> c_int;
    pub fn bch2_dev_offline(c: *mut Fs, ca: *mut Dev, flags: c_int,
                            err: *mut Printbuf) -> c_int;
    pub fn bch2_dev_set_state(c: *mut Fs, ca: *mut Dev,
                              new_state: c::bch_member_state, flags: c_int,
                              err: *mut Printbuf) -> c_int;
    pub fn __bch2_dev_set_state(c: *mut Fs, ca: *mut Dev,
                                new_state: c::bch_member_state, flags: c_int,
                                err: *mut Printbuf) -> c_int;
    pub fn bch2_dev_lookup(c: *mut Fs, name: *const c_char) -> *mut Dev;
    pub fn bch2_dev_may_offline(c: *mut Fs, ca: *mut Dev, flags: c_int,
                                err: *mut Printbuf) -> c_int;
    pub fn __bch2_dev_offline(c: *mut Fs, ca: *mut Dev);
    pub fn bch2_dev_read_identity(bdev: *mut c::block_device,
                                  name: *mut c_char, name_size: usize,
                                  model: *mut c_char, model_size: usize,
                                  serial: *mut c_char, serial_size: usize);
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

    let member_seq = c::bcachefs_metadata_version::member_seq.0;
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

/// # Safety
/// @fs is a superblock.
#[c_export]
pub unsafe fn bch2_dev_in_fs(fs: *mut Head<c::bch_sb>, fs_bdev: Option<&c::block_device>,
                             sb: &c::bch_sb_handle, opts: &c::bch_opts) -> c_int {
    // SAFETY: the caller's
    let fs = unsafe { c::bch_sb::<Validated>::from_ffi(fs) };
    ret_to_c(dev_in_fs(fs, fs_bdev, sb, opts))
}

/// The memory of a new bch_dev of @c: bch_dev::init()'s - its Rust-typed
/// fields' initializers run, everything else zeroed - for __bch2_dev_alloc()
/// to go on with. NULL if out of memory.
#[c_export]
pub fn bch2_dev_alloc_mem(c: &crate::util::ffi::Opaque<Fs>) -> *mut Dev {
    crate::util::alloc::alloc_pinned(Dev::init(crate::fs::Fs::from_c(c)))
        .unwrap_or(core::ptr::null_mut())
}

/// Drop @ca and free it, from its kobject's release.
///
/// # Safety
/// @ca is from bch2_dev_alloc_mem(), and nothing uses it after.
#[c_export]
pub unsafe fn bch2_dev_free_mem(ca: *mut Dev) {
    unsafe { crate::util::alloc::free_pinned(ca) }
}

impl StateLockWrite<'_> {
    /// @ca set to @state, the state lock held; why not, if it can't be, in @err.
    pub fn dev_set_state(&self, ca: &Dev, state: c::bch_member_state, flags: u32, err: &mut Printbuf)
        -> Result<(), BchError>
    {
        let ca = core::ptr::from_ref(ca).cast_mut();
        // SAFETY: a device of the filesystem, its state lock held
        ret_to_result_void(unsafe {
            c::__bch2_dev_set_state(self.fs().as_ptr(), ca, state, flags as c_int, err.as_raw())
        })
    }

    /// Whether the filesystem can do without @ca; why not, in @err.
    pub fn dev_may_offline(&self, ca: &Dev, flags: u32, err: &mut Printbuf) -> Result<(), BchError> {
        let ca = core::ptr::from_ref(ca).cast_mut();
        ret_to_result_void(unsafe {
            c::bch2_dev_may_offline(self.fs().as_ptr(), ca, flags as c_int, err.as_raw())
        })
    }

    /// @ca's block device closed: __bch2_dev_offline().
    pub fn dev_offline(&self, ca: &Dev) {
        let ca = core::ptr::from_ref(ca).cast_mut();
        unsafe { c::__bch2_dev_offline(self.fs().as_ptr(), ca) }
    }
}

impl Fs {
    /// @ca set to @state - dirty pagecache written back first if it's leaving
    /// the allocator, then under the state lock; why not, if it can't be, in @err.
    pub fn dev_set_state(&self, ca: &Dev, state: c::bch_member_state, flags: u32,
                         err: Option<&mut Printbuf>) -> Result<(), BchError>
    {
        let ca = core::ptr::from_ref(ca).cast_mut();
        let err = err.map_or(core::ptr::null_mut(), |e| e.as_raw());
        // SAFETY: a device of the filesystem
        ret_to_result_void(unsafe { c::bch2_dev_set_state(self.as_ptr(), ca, state, flags as c_int, err) })
    }

    /// @devs by name, space separated; one there isn't, by its number.
    #[cold]
    pub fn devs_list_to_text(&self, out: &mut Printbuf, devs: &c::bch_devs_list) {
        // room first: under RCU it can only allocate atomically
        out.make_room_gfp(1024, c::GFP_KERNEL);
        let mut out = out.atomic();
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

#[cold]
#[c_export]
pub fn bch2_devs_list_to_text(out: &mut Printbuf, c: &Fs, d: &c::bch_devs_list) {
    c.devs_list_to_text(out, d)
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

#[cfg(not(CONFIG_BCACHEFS_NO_LATENCY_ACCT))]
#[c_export]
pub fn bch2_dev_congested_read(ca: &Dev, now: u64) -> u32 {
    ca.congested_read(now)
}

// ── blk_holder_ops ────────────────────────────────────────────────────────

#[cfg(kernel)]
pub(crate) static HOLDER_OPS: c::blk_holder_ops = c::blk_holder_ops {
    mark_dead: Some(bdev_mark_dead),
    sync: Some(bdev_sync),
    ..unsafe { core::mem::zeroed() }
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
    let started = |fs: &Fs| fs.flags.get(c::bch_fs_flags::BCH_FS_started);
    let Ok(()) = fs.ro_ref_events.wait_until(Sleep::UNINTERRUPTIBLE, || {
        (started(fs) || fs.flags.get(c::bch_fs_flags::BCH_FS_stopping)).then_some(())
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
        unsafe {
            let disk = (*self.disk_sb.bdev).bd_disk;
            #[cfg(kernel)]
            let sectors = (*(*disk).part0).bd_nr_sectors;
            #[cfg(not(kernel))]
            let sectors = c::get_capacity(disk);
            sectors as u64
        }
    }

    fn will_resize_on_mount(&self) -> bool {
        let mi = self.mi.get();
        mi.resize_on_mount != 0 && mi.nbuckets < self.disk_sectors() / u64::from(mi.bucket_size)
    }
}

impl Fs {
    pub(crate) fn will_resize_on_mount(&self) -> bool {
        self.online_members().any(|ca| ca.will_resize_on_mount())
    }

    pub fn resize_on_mount(&self) -> Result<(), BchError> {
        for ca in self.online_members() {
            if !ca.will_resize_on_mount() {
                continue;
            }

            let mi = ca.mi.get();
            let old_nbuckets = mi.nbuckets;
            let new_nbuckets = ca.disk_sectors() / u64::from(mi.bucket_size);

            crate::bch_info_dev!(&ca, "resizing to size {}", new_nbuckets * u64::from(mi.bucket_size));
            crate::bch_err_fn_dev!(&ca, ca.buckets_resize(new_nbuckets))?;

            {
                let mut sb = self.sb_lock();
                let m = sb.fs_member_mut(ca.dev_idx.into());
                m.nbuckets = new_nbuckets.into();
                m.set_resize_on_mount(false);

                let s = sb.sb_mut();
                s.features[0] = (s.features[0].get() & !(1 << c::bch_sb_feature::BCH_FEATURE_small_image.0)).into();
                self.write_super(&mut sb);
            }

            if ca.mi.get().freespace_initialized != 0 {
                let ca = core::ptr::from_ref::<Dev>(&ca).cast_mut();
                ret_to_result_void(unsafe { __bch2_dev_resize_alloc(ca, old_nbuckets, new_nbuckets) })?;
            }
        }
        Ok(())
    }
}

#[c_export]
pub fn bch2_fs_resize_on_mount(c: &Fs) -> c_int {
    ret_to_c(c.resize_on_mount())
}
