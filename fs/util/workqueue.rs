// SPDX-License-Identifier: GPL-2.0

//! Work run after a delay: struct delayed_work, in a struct Rust defines and
//! C may share. The kernel crate's DelayedWork is for Arc-owned work items,
//! and userspace has none: this is the bare struct, initialized with its
//! function as C's INIT_DELAYED_WORK() - in-kernel with a lockdep class per
//! call site, as the kernel crate's. fs_delayed_work!() declares one in
//! bch_fs.

use crate::c;
use core::cell::UnsafeCell;
use core::ffi::c_ulong;
use core::marker::PhantomPinned;

pub type WorkFn = unsafe extern "C" fn(*mut c::work_struct);

#[repr(transparent)]
pub struct DelayedWork {
    work: UnsafeCell<c::delayed_work>,
    _pin: PhantomPinned,
}

// SAFETY: the workqueue's to synchronize, from any thread
unsafe impl Send for DelayedWork {}
unsafe impl Sync for DelayedWork {}

impl DelayedWork {
    /// The work, and its timer, each with a lockdep class of its own.
    #[cfg(kernel)]
    #[doc(hidden)]
    pub fn new(
        func:       WorkFn,
        name:       &'static core::ffi::CStr,
        key:        core::pin::Pin<&'static kernel::sync::LockClassKey>,
        timer_name: &'static core::ffi::CStr,
        timer_key:  core::pin::Pin<&'static kernel::sync::LockClassKey>,
    ) -> impl ::pin_init::PinInit<Self> {
        // SAFETY: the work and its timer, all there is, initialized in place -
        // it's pinned - as the kernel crate's DelayedWork::new()
        unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut Self| {
                let dwork = slot.cast::<c::delayed_work>();
                c::init_work_with_key(&raw mut (*dwork).work, Some(func), false,
                                      name.as_ptr().cast(), key.as_ptr());
                c::timer_init_key(&raw mut (*dwork).timer, Some(c::delayed_work_timer_fn),
                                  c::TIMER_IRQSAFE, timer_name.as_ptr().cast(), timer_key.as_ptr());
                Ok(())
            })
        }
    }

    /// INIT_DELAYED_WORK(), as the shim has it: no lockdep.
    #[cfg(not(kernel))]
    #[doc(hidden)]
    pub fn new(func: WorkFn) -> impl ::pin_init::PinInit<Self> {
        // SAFETY: initialized in place, all of it
        unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut Self| {
                c::bch2_init_delayed_work(slot.cast(), Some(func));
                Ok(())
            })
        }
    }

    pub fn as_ptr(&self) -> *mut c::delayed_work {
        self.work.get()
    }

    /// Run on @wq after @delay jiffies: queue_delayed_work(). False if it was
    /// already queued - then that run is the one there'll be.
    pub fn queue(&self, wq: *mut c::workqueue_struct, delay: c_ulong) -> bool {
        unsafe { c::bch2_queue_delayed_work(wq, self.as_ptr(), delay) }
    }

    /// Not queued any more, and not running - waiting out a run that is:
    /// cancel_delayed_work_sync(). Whether it was pending.
    pub fn cancel_sync(&self) -> bool {
        unsafe { c::bch2_cancel_delayed_work_sync(self.as_ptr()) }
    }
}

/// system_long_wq: for work that may run long.
pub fn system_long_wq() -> *mut c::workqueue_struct {
    unsafe { c::bch2_system_long_wq() }
}
