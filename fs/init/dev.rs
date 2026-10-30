// SPDX-License-Identifier: GPL-2.0

//! Devices: dev.c's, as it converts.
//!
//! So far devs_list_to_text(), its congestion, its state set through C's
//! dev_set_state(), and a bch_dev's memory: Rust allocates it, as bch_fs's,
//! so its Rust-typed fields have initializers - the bucket bitmaps' Locks -
//! and it's dropped whole when it's freed: after C's teardown, which leaves
//! what it freed nulled for those fields' Drops to find.

use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::fs::{Fs, StateLockWrite};
use crate::prt;
use crate::util::rcu::read_lock;
use crate::util::{Nsecs, Printbuf};
use core::ffi::c_int;
use core::sync::atomic::Ordering::Relaxed;
use cstruct_macros::c_export;

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
