// SPDX-License-Identifier: GPL-2.0

//! Workqueues, and work items: run holding a ref, or after a delay.
//!
//! Workqueue owns a workqueue: alloc_workqueue(), destroy_workqueue() when
//! it drops - a field of a struct C shares, as its struct workqueue_struct *,
//! null until it's made. Queue is one by reference, what work is queued on: a
//! Workqueue's, a system queue, or one C made.
//!
//! ReffedWork<F, R> is struct work_struct, queued and run holding refs,
//! its field's type saying all of it: F the function it runs, a plain fn of
//! the parent, and R the refs it takes, a path from the parent -
//! field!(super.writes), the write refs of the Fs it's in (util/place.rs's
//! paths) - or a pair of them, or OnQueue, the parent's own queue it runs on.
//! Its parent's #[derive(Init)] makes it, placed at its field: so C's pointer
//! to it is had back as the ReffedWork, and its container() as the parent F
//! runs on. CancelledWork<F> holds no ref: its drop cancels it, so it's its
//! parent's first field. DelayedWork<F, R> is a ReffedWork after a delay:
//! all in work.rs.
//!
//! RawWork and RawDelayedWork are struct work_struct and struct delayed_work,
//! bare - under the works above, or for a parent not made by
//! #[derive(Init)] - queued and cancelled through them, the shim's calls here
//! alone: initialized with their function as C's INIT_WORK() and
//! INIT_DELAYED_WORK().
//!
//! The kernel port's interface, over the shim's workqueues: queues.rs.

mod queues;

pub use queues::{delayed_work, work_struct};
#[allow(non_camel_case_types)]
pub type workqueue_struct = queues::Workqueue;

use crate::util::alloc::AllocError;
use crate::util::ffi::Opaque;
use bitflags::bitflags;
use core::cell::UnsafeCell;
use core::ffi::CStr;
use crate::util::ffi::c_ulong;
use core::marker::PhantomPinned;
use core::ptr::NonNull;

bitflags! {
    /// alloc_workqueue()'s WQ_* flags.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct WqFlags: u32 {
        /// Not bound to a CPU.
        const UNBOUND       = 1 << 1;
        /// Frozen during suspend.
        const FREEZABLE     = 1 << 2;
        /// Has a rescuer, for work memory reclaim waits on.
        const MEM_RECLAIM   = 1 << 3;
        const HIGHPRI       = 1 << 4;
        const CPU_INTENSIVE = 1 << 5;
        const PERCPU        = 1 << 8;
    }
}

/// A workqueue Rust owns, destroyed on drop - none until it's made, as a
/// struct's field C sees as its struct workqueue_struct *.
#[repr(transparent)]
#[derive(Default)]
pub struct Workqueue(Option<NonNull<workqueue_struct>>);

// SAFETY: queued on, and destroyed, from any thread
unsafe impl Send for Workqueue {}
unsafe impl Sync for Workqueue {}

// SAFETY: none, a null pointer
unsafe impl ::pin_init::Zeroable for Workqueue {}

impl crate::util::late::LateInit for Workqueue {
    fn is_made(&self) -> bool {
        self.is_some()
    }
}

impl crate::util::late::LateStop for Workqueue {
    fn stop(&self) {
        self.drain()
    }
}

impl Workqueue {
    /// Workqueue @name, running @max_active of its work at once (0: the
    /// default): alloc_workqueue(). Its failure is the caller's to name, as
    /// C's are.
    pub fn new(name: &CStr, flags: WqFlags, max_active: i32) -> Result<Self, AllocError> {
        // SAFETY: plain; the name is copied
        let wq = unsafe { queues::alloc_workqueue_name(name.as_ptr(), flags.bits(), max_active) };
        NonNull::new(wq).map(|wq| Workqueue(Some(wq))).ok_or(AllocError)
    }

    /// Whether it's been made.
    pub fn is_some(&self) -> bool {
        self.0.is_some()
    }

    /// Kept as a pointer, for from_raw() to have back.
    pub fn into_raw(self) -> *mut workqueue_struct {
        core::mem::ManuallyDrop::new(self).0.map_or(core::ptr::null_mut(), NonNull::as_ptr)
    }

    /// # Safety
    /// @wq is into_raw()'s, had back once.
    pub unsafe fn from_raw(wq: *mut workqueue_struct) -> Self {
        Workqueue(NonNull::new(wq))
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
            unsafe { queues::destroy_workqueue(wq.as_ptr()) };
        }
    }
}

/// A workqueue by reference: a Workqueue's, a system queue, or one C made and
/// outlives its use.
#[repr(transparent)]
pub struct Queue(Opaque<workqueue_struct>);

// SAFETY: work is queued on a workqueue from any thread
unsafe impl Sync for Queue {}

impl Queue {
    /// # Safety
    /// @wq is a workqueue that lives for 'a.
    pub unsafe fn from_raw<'a>(wq: *mut workqueue_struct) -> &'a Queue {
        // SAFETY: the caller's; repr(transparent) over the C struct
        unsafe { &*wq.cast() }
    }

    /// Everything queued run, and what that queues, until it's empty:
    /// drain_workqueue().
    pub fn drain(&self) {
        unsafe { queues::drain_workqueue(self.as_ptr()) }
    }

    pub fn as_ptr(&self) -> *mut workqueue_struct {
        self.0.as_ptr()
    }

    /// system_long_wq: for work that may run long.
    pub fn system_long() -> &'static Queue {
        // SAFETY: the system's, for as long as there is a system
        unsafe { Self::from_raw(&raw mut queues::EVENTS_LONG) }
    }

    /// system_percpu_wq: per-cpu, what schedule_work_on() queues on.
    pub fn system_percpu() -> &'static Queue {
        // SAFETY: the system's
        unsafe { Self::from_raw(&raw mut queues::EVENTS) }
    }

    /// system_dfl_wq: the default, unbound.
    pub fn system_dfl() -> &'static Queue {
        // SAFETY: the system's
        unsafe { Self::from_raw(&raw mut queues::EVENTS_UNBOUND) }
    }

    /// system_highpri_wq: per-cpu, high priority.
    pub fn system_highpri() -> &'static Queue {
        // SAFETY: the system's
        unsafe { Self::from_raw(&raw mut queues::EVENTS_HIGHPRI) }
    }
}

/// The running worker's description: userspace's workers have none.
pub fn set_worker_desc(_desc: &CStr) {}

pub type WorkFn = unsafe extern "C" fn(*mut work_struct);

/// A work_struct at a fixed place in Self, running Self::run(): new_work!()
/// with run_raw::<Self>, the trampoline, which run() is inlined into.
pub trait WorkAt {
    /// offset_of!() the work_struct
    const WORK: usize;

    /// # Safety
    /// @this was queued, as its queuer's contract says.
    unsafe fn run(this: NonNull<Self>);
}

#[doc(hidden)]
pub unsafe extern "C" fn run_raw<T: WorkAt>(work: *mut work_struct) {
    // SAFETY: T's work_struct, at T::WORK
    unsafe { T::run(NonNull::new_unchecked(work.byte_sub(T::WORK).cast())) }
}

/// A RawWork's initializer, running C's @func: INIT_WORK(). @name is the
/// kernel port's, for its lockdep class.
#[macro_export]
macro_rules! new_work {
    ($func:expr, $name:literal $(,)?) => {
        $crate::os::workqueue::RawWork::new($func)
    };
}

/// A RawDelayedWork's initializer, running C's @func: INIT_DELAYED_WORK(). @name
/// is the kernel port's, for its lockdep classes.
#[macro_export]
macro_rules! new_delayed_work {
    ($func:expr, $name:literal $(,)?) => {
        $crate::os::workqueue::RawDelayedWork::new($func)
    };
}

#[path = "../work.rs"]
mod work;
pub(crate) use work::{CancelledWork, DelayedWork, OnQueue, Own, ReffedWork, WorkFunc};

#[repr(transparent)]
pub struct RawWork {
    work: UnsafeCell<work_struct>,
    _pin: PhantomPinned,
}

unsafe impl Send for RawWork {}
unsafe impl Sync for RawWork {}

impl RawWork {
    /// INIT_WORK().
    #[doc(hidden)]
    pub fn new(func: WorkFn) -> impl ::pin_init::PinInit<Self> {
        unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut Self| {
                queues::init_work(slot.cast(), func);
                Ok(())
            })
        }
    }

    fn as_ptr(&self) -> *mut work_struct {
        self.work.get()
    }

    /// Run on @wq: queue_work(). False if it was queued already - then that
    /// run is the one there'll be.
    ///
    /// # Safety
    /// It lives until its function has run: the workqueue has it by address
    /// till then.
    pub unsafe fn queue(&self, wq: &Queue) -> bool {
        unsafe { queues::queue_work(wq.as_ptr(), self.as_ptr()) }
    }

    /// queue(), to run on @cpu: queue_work_on(). There's no CPU to choose
    /// here.
    ///
    /// # Safety
    /// As queue()'s.
    pub unsafe fn queue_on(&self, wq: &Queue, _cpu: u32) -> bool {
        unsafe { self.queue(wq) }
    }

    /// Its last queueing run: flush_work().
    pub fn flush(&self) {
        unsafe { queues::flush_work(self.as_ptr()) };
    }

    /// Not queued any more, and not running - waiting out a run that is:
    /// cancel_work_sync(). Whether it was pending.
    pub fn cancel_sync(&self) -> bool {
        unsafe { queues::cancel_work_sync(self.as_ptr()) }
    }

    /// Whether it's queued or running, as of the look: work_busy(). The
    /// shim's workqueues don't say.
    pub fn busy(&self) -> bool {
        false
    }
}

#[repr(transparent)]
pub struct RawDelayedWork {
    work: UnsafeCell<delayed_work>,
    _pin: PhantomPinned,
}

// SAFETY: the workqueue's to synchronize, from any thread
unsafe impl Send for RawDelayedWork {}
unsafe impl Sync for RawDelayedWork {}

impl RawDelayedWork {
    /// INIT_DELAYED_WORK().
    #[doc(hidden)]
    pub fn new(func: WorkFn) -> impl ::pin_init::PinInit<Self> {
        // SAFETY: initialized in place, all of it - its work_struct
        unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut Self| {
                queues::init_work(&raw mut (*slot.cast::<delayed_work>()).work, func);
                Ok(())
            })
        }
    }

    pub fn as_ptr(&self) -> *mut delayed_work {
        self.work.get()
    }

    /// Run on @wq after @delay jiffies: queue_delayed_work(). False if it was
    /// already queued - then that run is the one there'll be.
    ///
    /// # Safety
    /// As RawWork::queue()'s.
    pub unsafe fn queue(&self, wq: &Queue, delay: c_ulong) -> bool {
        unsafe { queues::queue_delayed_work(wq.as_ptr(), self.as_ptr(), delay) }
    }

    /// Run on @wq after @delay jiffies, from now - re-armed if it was already
    /// queued: mod_delayed_work().
    ///
    /// # Safety
    /// As RawWork::queue()'s.
    pub unsafe fn modify(&self, wq: &Queue, delay: c_ulong) -> bool {
        unsafe { queues::mod_delayed_work(wq.as_ptr(), self.as_ptr(), delay) }
    }

    /// Not queued any more - a run under way still runs: cancel_delayed_work().
    pub fn cancel(&self) -> bool {
        unsafe { queues::cancel_delayed_work(self.as_ptr()) }
    }

    /// Not queued any more, and not running - waiting out a run that is:
    /// cancel_delayed_work_sync(). Whether it was pending.
    pub fn cancel_sync(&self) -> bool {
        unsafe { queues::cancel_delayed_work_sync(self.as_ptr()) }
    }
}
