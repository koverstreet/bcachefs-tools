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
//! paths) - or a pair of them. Its parent's #[derive(Init)] makes it, placed
//! at its field: so C's pointer to it is had back as the ReffedWork, and its
//! container() as the parent F runs on.
//!
//! DelayedWork is struct delayed_work, in a struct Rust defines and C may
//! share: the bare struct, initialized with its function as C's
//! INIT_DELAYED_WORK(). fs_delayed_work!() declares one in bch_fs.
//!
//! The kernel port's interface, over the shim's workqueues: queues.rs.

mod queues;

use crate::c;
use crate::util::alloc::AllocError;
use crate::util::ffi::Opaque;
use bitflags::bitflags;
use core::cell::UnsafeCell;
use core::ffi::{c_int, c_ulong, CStr};
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
pub struct Workqueue(Option<NonNull<c::workqueue_struct>>);

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
    pub fn new(name: &CStr, flags: WqFlags, max_active: c_int) -> Result<Self, AllocError> {
        // SAFETY: plain; the name is copied
        let wq = unsafe { queues::alloc_workqueue_name(name.as_ptr(), flags.bits(), max_active) };
        NonNull::new(wq).map(|wq| Workqueue(Some(wq))).ok_or(AllocError)
    }

    /// Whether it's been made.
    pub fn is_some(&self) -> bool {
        self.0.is_some()
    }

    /// Kept as a pointer, for from_raw() to have back.
    pub fn into_raw(self) -> *mut c::workqueue_struct {
        core::mem::ManuallyDrop::new(self).0.map_or(core::ptr::null_mut(), NonNull::as_ptr)
    }

    /// # Safety
    /// @wq is into_raw()'s, had back once.
    pub unsafe fn from_raw(wq: *mut c::workqueue_struct) -> Self {
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

    /// Everything queued run, and what that queues, until it's empty:
    /// drain_workqueue().
    pub fn drain(&self) {
        unsafe { queues::drain_workqueue(self.as_ptr()) }
    }

    pub fn as_ptr(&self) -> *mut c::workqueue_struct {
        self.0.as_ptr()
    }

    /// system_long_wq: for work that may run long.
    pub fn system_long() -> &'static Queue {
        // SAFETY: the system's, for as long as there is a system
        unsafe { Self::from_raw((&raw mut queues::EVENTS_LONG).cast()) }
    }

    /// system_wq: per-cpu, what schedule_work_on() queues on.
    pub fn system() -> &'static Queue {
        // SAFETY: the system's
        unsafe { Self::from_raw((&raw mut queues::EVENTS).cast()) }
    }

    /// system_dfl_wq: the default, unbound.
    pub fn system_dfl() -> &'static Queue {
        // SAFETY: the system's
        unsafe { Self::from_raw((&raw mut queues::EVENTS_UNBOUND).cast()) }
    }

    /// system_highpri_wq: per-cpu, high priority.
    pub fn system_highpri() -> &'static Queue {
        // SAFETY: the system's
        unsafe { Self::from_raw((&raw mut queues::EVENTS_HIGHPRI).cast()) }
    }

    /// Run @work here: queue_work(). False if it was queued already - then
    /// that run is the one there'll be.
    ///
    /// # Safety
    /// @work is initialized, and lives until its function has run.
    pub unsafe fn queue_work(&self, work: *mut c::work_struct) -> bool {
        // SAFETY: the caller's
        unsafe { queues::queue_work(self.as_ptr(), work) }
    }

    /// queue_work(), to run on @cpu: queue_work_on(). There's no CPU to
    /// choose here.
    ///
    /// # Safety
    /// As queue_work()'s.
    pub unsafe fn queue_work_on(&self, _cpu: u32, work: *mut c::work_struct) -> bool {
        unsafe { self.queue_work(work) }
    }
}

/// Wait for @work's last queueing to have run: flush_work().
///
/// # Safety
/// @work is initialized.
pub unsafe fn flush_work(work: *mut c::work_struct) {
    unsafe { queues::flush_work(work) };
}

/// Not queued any more, and not running - waiting out a run that is:
/// cancel_work_sync(). Whether it was pending.
///
/// # Safety
/// @work is initialized.
pub unsafe fn cancel_work_sync(work: *mut c::work_struct) -> bool {
    unsafe { queues::cancel_work_sync(work) }
}

/// The running worker's description: userspace's workers have none.
pub fn set_worker_desc(_desc: &CStr) {}

/// Whether @work is queued or running, as of the look: work_busy(). The shim's
/// workqueues don't say.
///
/// # Safety
/// @work is initialized.
pub unsafe fn work_busy(_work: *mut c::work_struct) -> bool {
    false
}

pub type WorkFn = unsafe extern "C" fn(*mut c::work_struct);

/// A work_struct at a fixed place in Self, running Self::run(): new_work!()
/// with run_raw::<Self>, the trampoline, which run() is inlined into.
pub trait RawWork {
    /// offset_of!() the work_struct
    const WORK: usize;

    /// # Safety
    /// @this was queued, as its queuer's contract says.
    unsafe fn run(this: NonNull<Self>);
}

#[doc(hidden)]
pub unsafe extern "C" fn run_raw<T: RawWork>(work: *mut c::work_struct) {
    // SAFETY: T's work_struct, at T::WORK
    unsafe { T::run(NonNull::new_unchecked(work.byte_sub(T::WORK).cast())) }
}

/// INIT_WORK(): a C work_struct, in place, running C's @func: new_work!()'s.
#[doc(hidden)]
pub fn work_init(func: WorkFn) -> impl ::pin_init::PinInit<c::work_struct> {
    // SAFETY: initialized in place, all of it
    unsafe {
        ::pin_init::pin_init_from_closure(move |slot: *mut c::work_struct| {
            queues::init_work(slot, func);
            Ok(())
        })
    }
}

/// A C work_struct's initializer, running C's @func: INIT_WORK(). @name is
/// the kernel port's, for its lockdep class.
#[macro_export]
macro_rules! new_work {
    ($func:expr, $name:literal $(,)?) => {
        $crate::os::workqueue::work_init($func)
    };
}

/// A DelayedWork's initializer, running C's @func: INIT_DELAYED_WORK(). @name
/// is the kernel port's, for its lockdep classes.
#[macro_export]
macro_rules! new_delayed_work {
    ($func:expr, $name:literal $(,)?) => {
        $crate::os::workqueue::DelayedWork::new($func)
    };
}

#[path = "../work.rs"]
mod work;
pub(crate) use work::{ReffedWork, WorkFunc};

#[repr(transparent)]
pub struct DelayedWork {
    work: UnsafeCell<c::delayed_work>,
    _pin: PhantomPinned,
}

// SAFETY: the workqueue's to synchronize, from any thread
unsafe impl Send for DelayedWork {}
unsafe impl Sync for DelayedWork {}

impl DelayedWork {
    /// INIT_DELAYED_WORK().
    #[doc(hidden)]
    pub fn new(func: WorkFn) -> impl ::pin_init::PinInit<Self> {
        // SAFETY: initialized in place, all of it - its work_struct
        unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut Self| {
                queues::init_work(&raw mut (*slot.cast::<c::delayed_work>()).work, func);
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
        unsafe { queues::queue_delayed_work(wq.as_ptr(), self.as_ptr(), delay) }
    }

    /// Run on @wq after @delay jiffies, from now - re-armed if it was already
    /// queued: mod_delayed_work().
    pub fn modify(&self, wq: &Queue, delay: c_ulong) -> bool {
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
