// SPDX-License-Identifier: GPL-2.0

//! Devices: dev.c's, as it converts.
//!
//! So far devs_list_to_text(), its congestion, its state set through C's
//! dev_set_state(), and a bch_dev's memory: Rust allocates it, as bch_fs's,
//! so its Rust-typed fields have initializers - the bucket bitmaps' Locks -
//! and it's dropped whole when it's freed: after C's teardown, which leaves
//! what it freed nulled for those fields' Drops to find.

#![allow(non_camel_case_types)]

use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::fs::{Fs, StateLockWrite};
use crate::prt;
use crate::util::rcu::read_lock;
use crate::util::{Nsecs, Printbuf};
use core::ffi::c_int;
use core::sync::atomic::Ordering::Relaxed;
use cstruct_macros::{c_export, CStruct, bitfield, c_extern, c_typedef, TypeInfo};
use crate::util::vstructs::Head;
use crate::types::{DArray, bits_to_longs, c_default};
use crate::util::bio::{Bio, BioBufPtr};
use crate::util::bitmap::{AtomicBitArray, BitArray};
use crate::util::ffi::c_char;
use nestify::nest;

#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_sb_handle_holder {
    pub c: *mut c::bch_fs,
}
c_default!(bch_sb_handle_holder);

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
    pub holder: *mut c::bch_sb_handle_holder,
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
    pub fn bch2_dev_in_fs(fs: *mut Head<c::bch_sb>, fs_bdev: *mut c::block_device,
                          sb: *mut c::bch_sb_handle, opts: *mut c::bch_opts) -> c_int;
    pub fn bch2_dev_add(c: *mut c::bch_fs, path: *const c_char, err: *mut c::printbuf) -> c_int;
    pub fn bch2_dev_add_initialize(c: *mut c::bch_fs, ca: *mut c::bch_dev) -> c_int;
    pub fn bch2_dev_resize(c: *mut c::bch_fs, ca: *mut c::bch_dev, nbuckets: u64,
                           err: *mut c::printbuf) -> c_int;
    pub fn bch2_dev_remove(c: *mut c::bch_fs, ca: *mut c::bch_dev, flags: c_int,
                           err: *mut c::printbuf) -> c_int;
    pub fn bch2_dev_online(c: *mut c::bch_fs, path: *const c_char, err: *mut c::printbuf) -> c_int;
    pub fn bch2_dev_offline(c: *mut c::bch_fs, ca: *mut c::bch_dev, flags: c_int,
                            err: *mut c::printbuf) -> c_int;
    pub fn bch2_dev_set_state(c: *mut c::bch_fs, ca: *mut c::bch_dev,
                              new_state: c::bch_member_state, flags: c_int,
                              err: *mut c::printbuf) -> c_int;
    pub fn __bch2_dev_set_state(c: *mut c::bch_fs, ca: *mut c::bch_dev,
                                new_state: c::bch_member_state, flags: c_int,
                                err: *mut c::printbuf) -> c_int;
    pub fn bch2_dev_lookup(c: *mut c::bch_fs, name: *const c_char) -> *mut c::bch_dev;
    pub static bch2_sb_handle_bdev_ops: c::blk_holder_ops;
    pub fn bch2_dev_read_identity(bdev: *mut c::block_device,
                                  name: *mut c_char, name_size: usize,
                                  model: *mut c_char, model_size: usize,
                                  serial: *mut c_char, serial_size: usize);
}

/// The memory of a new bch_dev of @c: bch_dev::init()'s - its Rust-typed
/// fields' initializers run, everything else zeroed - for __bch2_dev_alloc()
/// to go on with. NULL if out of memory.
#[c_export]
pub fn bch2_dev_alloc_mem(c: &crate::util::ffi::Opaque<c::bch_fs>) -> *mut c::bch_dev {
    crate::util::alloc::alloc_pinned(c::bch_dev::init(crate::fs::Fs::from_c(c)))
        .unwrap_or(core::ptr::null_mut())
}

/// Drop @ca and free it, from its kobject's release.
///
/// # Safety
/// @ca is from bch2_dev_alloc_mem(), and nothing uses it after.
#[c_export]
pub unsafe fn bch2_dev_free_mem(ca: *mut c::bch_dev) {
    unsafe { crate::util::alloc::free_pinned(ca) }
}

impl StateLockWrite<'_> {
    /// @ca set to @state, the state lock held; why not, if it can't be, in @err.
    pub fn dev_set_state(&self, ca: &c::bch_dev, state: c::bch_member_state, flags: u32, err: &mut Printbuf)
        -> Result<(), BchError>
    {
        let ca = core::ptr::from_ref(ca).cast_mut();
        // SAFETY: a device of the filesystem, its state lock held
        ret_to_result_void(unsafe {
            c::__bch2_dev_set_state(self.fs().as_ptr(), ca, state, flags as c_int, err.as_raw())
        })
    }
}

impl Fs {
    /// @ca set to @state - dirty pagecache written back first if it's leaving
    /// the allocator, then under the state lock; why not, if it can't be, in @err.
    pub fn dev_set_state(&self, ca: &c::bch_dev, state: c::bch_member_state, flags: u32,
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
impl c::bch_dev {
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
impl c::bch_dev {
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
pub fn bch2_dev_congested_read(ca: &c::bch_dev, now: u64) -> u32 {
    ca.congested_read(now)
}
