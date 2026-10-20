// SPDX-License-Identifier: GPL-2.0

//! Workqueues, and work run after a delay.
//!
//! Workqueue owns a workqueue: alloc_workqueue(), destroy_workqueue() when
//! it drops - a field of a struct C shares, as its struct workqueue_struct *,
//! null until it's made. Queue is one by reference, what work is queued on: a
//! Workqueue's, a system queue, or one C made.
//!
//! DelayedWork is struct delayed_work, in a struct Rust defines and C may
//! share. The kernel crate's DelayedWork is for Arc-owned work items, and
//! userspace has none: this is the bare struct, initialized with its function
//! as C's INIT_DELAYED_WORK() - in-kernel with a lockdep class per call site,
//! as the kernel crate's. fs_delayed_work!() declares one in bch_fs.

use crate::c;
use crate::util::alloc::AllocError;
use crate::util::ffi::Opaque;
use crate::util::Printbuf;
use bitflags::bitflags;
use core::cell::UnsafeCell;
use core::ffi::{c_int, c_ulong};
use core::fmt;
use core::marker::PhantomPinned;
use core::ptr::NonNull;

bitflags! {
    /// alloc_workqueue()'s WQ_* flags: util.c checks they're C's.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct WqFlags: u32 {
        /// Not bound to a CPU.
        const UNBOUND       = c::BCH_WQ_UNBOUND;
        /// Frozen during suspend.
        const FREEZABLE     = c::BCH_WQ_FREEZABLE;
        /// Has a rescuer, for work memory reclaim waits on.
        const MEM_RECLAIM   = c::BCH_WQ_MEM_RECLAIM;
        const HIGHPRI       = c::BCH_WQ_HIGHPRI;
        const CPU_INTENSIVE = c::BCH_WQ_CPU_INTENSIVE;
    }
}

/// A workqueue Rust owns, destroyed on drop - none until it's made, as a
/// struct's field C sees as its struct workqueue_struct *.
#[repr(transparent)]
#[derive(Default)]
pub struct Workqueue(Option<NonNull<c::workqueue_struct>>);

// SAFETY: queued on, and destroyed, from any thread
unsafe impl Send for Workqueue {}
unsafe impl Sync for Workqueue {}

// SAFETY: none, a null pointer
unsafe impl ::pin_init::Zeroable for Workqueue {}

impl Workqueue {
    /// Workqueue @name, running @max_active of its work at once (0: the
    /// default): alloc_workqueue(). Its failure is the caller's to name, as
    /// C's are.
    pub fn new(name: fmt::Arguments<'_>, flags: WqFlags, max_active: c_int) -> Result<Self, AllocError> {
        let mut name_buf = Printbuf::new();
        write!(name_buf, "{name}");
        // SAFETY: plain; the name is copied
        let wq = unsafe {
            c::bch2_alloc_workqueue(name_buf.as_c_str().as_ptr().cast(), flags.bits(), max_active)
        };
        NonNull::new(wq).map(|wq| Workqueue(Some(wq))).ok_or(AllocError)
    }

    /// Whether it's been made.
    pub fn is_some(&self) -> bool {
        self.0.is_some()
    }
}

/// Its queue - a workqueue not made is a bug to queue on.
impl core::ops::Deref for Workqueue {
    type Target = Queue;

    fn deref(&self) -> &Queue {
        let wq = self.0.expect("queueing on a workqueue not made");
        // SAFETY: ours, until we drop
        unsafe { Queue::from_raw(wq.as_ptr()) }
    }
}

impl Drop for Workqueue {
    /// destroy_workqueue(): what's queued runs first.
    fn drop(&mut self) {
        if let Some(wq) = self.0.take() {
            // SAFETY: ours, and nothing queues on it after this
            unsafe { c::bch2_destroy_workqueue(wq.as_ptr()) };
        }
    }
}

/// A workqueue by reference: a Workqueue's, a system queue, or one C made and
/// outlives its use.
#[repr(transparent)]
pub struct Queue(Opaque<c::workqueue_struct>);

// SAFETY: work is queued on a workqueue from any thread
unsafe impl Sync for Queue {}

impl Queue {
    /// # Safety
    /// @wq is a workqueue that lives for 'a.
    pub unsafe fn from_raw<'a>(wq: *mut c::workqueue_struct) -> &'a Queue {
        // SAFETY: the caller's; repr(transparent) over the C struct
        unsafe { &*wq.cast() }
    }

    pub fn as_ptr(&self) -> *mut c::workqueue_struct {
        self.0.as_ptr()
    }

    /// system_long_wq: for work that may run long.
    pub fn system_long() -> &'static Queue {
        // SAFETY: the system's, for as long as there is a system
        unsafe { Self::from_raw(c::bch2_system_long_wq()) }
    }

    /// system_dfl_wq: the default, unbound - system_unbound_wq before 6.17.
    pub fn system_dfl() -> &'static Queue {
        // SAFETY: the system's
        unsafe { Self::from_raw(c::bch2_system_dfl_wq()) }
    }

    /// Run @work here: queue_work(). False if it was queued already - then
    /// that run is the one there'll be.
    ///
    /// # Safety
    /// @work is initialized, and lives until its function has run.
    pub unsafe fn queue_work(&self, work: *mut c::work_struct) -> bool {
        // SAFETY: the caller's
        unsafe { c::bch2_queue_work(self.as_ptr(), work) }
    }
}

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
    pub fn queue(&self, wq: &Queue, delay: c_ulong) -> bool {
        unsafe { c::bch2_queue_delayed_work(wq.as_ptr(), self.as_ptr(), delay) }
    }

    /// Not queued any more, and not running - waiting out a run that is:
    /// cancel_delayed_work_sync(). Whether it was pending.
    pub fn cancel_sync(&self) -> bool {
        unsafe { c::bch2_cancel_delayed_work_sync(self.as_ptr()) }
    }
}
