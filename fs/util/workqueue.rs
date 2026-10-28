// SPDX-License-Identifier: GPL-2.0

//! Workqueues, and work items: run holding a ref, or after a delay.
//!
//! Workqueue owns a workqueue: alloc_workqueue(), destroy_workqueue() when
//! it drops - a field of a struct C shares, as its struct workqueue_struct *,
//! null until it's made. Queue is one by reference, what work is queued on: a
//! Workqueue's, a system queue, or one C made.
//!
//! ReffedWork<F, R> is struct work_struct, queued and run holding a ref,
//! its field's type saying all of it: F the function it runs, a plain fn of
//! the parent, and R the ref it takes, a path from the parent -
//! field!(super.writes), the write refs of the Fs it's in (util/place.rs's
//! paths). Its parent's #[derive(Init)] makes it, placed at its field: so
//! C's pointer to it is had back as the ReffedWork, and its container() as
//! the parent F runs on.
//!
//! DelayedWork is struct delayed_work, in a struct Rust defines and C may
//! share. The kernel crate's DelayedWork is for Arc-owned work items, and
//! userspace has none: this is the bare struct, initialized with its function
//! as C's INIT_DELAYED_WORK() - in-kernel with a lockdep class per call site,
//! as the kernel crate's. fs_delayed_work!() declares one in bch_fs.

use crate::c;
use crate::util::alloc::AllocError;
use crate::util::ffi::Opaque;
use crate::util::place::{Path, Place, Placed};
use crate::util::refcount::{RefGuard, TryRefcount};
use crate::util::Printbuf;
use bitflags::bitflags;
use core::cell::UnsafeCell;
use core::ffi::{c_int, c_ulong};
use core::fmt;
use core::marker::{PhantomData, PhantomPinned};
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
            unsafe { c::destroy_workqueue(wq.as_ptr()) };
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
        unsafe { Self::from_raw(c::system_long_wq) }
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

/// A ReffedWork's function as a type, and the field it runs from: for a
/// field `w: ReffedWork<f, R>`, the parent's #[derive(Init)] makes f - a
/// braced struct, a type only, beside the fn f - and this.
///
/// # Safety
/// At is that field's, and only its initializer makes a ReffedWork<Self, _>.
pub(crate) unsafe trait WorkFunc: Sized + 'static {
    #[doc(hidden)]
    type At: Place;

    fn run(p: &<Self::At as Place>::Base);
}

/// The struct F's ReffedWork is the field of.
pub(crate) type Parent<F> = <<F as WorkFunc>::At as Place>::Base;

/// struct work_struct, queued and run holding a ref; its field's type says
/// what: F the function, R the ref, by its path from the struct it's in -
///
/// ```text
/// wait_for_pagecache_and_delete_work: ReffedWork<delete_unlinked, field!(super.writes)>,
/// ```
///
/// runs delete_unlinked(&bch_fs_snapshots) holding a write ref of their Fs.
///
/// The parent's #[derive(Init)] makes it in place, holding the field's At:
/// so C's pointer to it is had back as the ReffedWork, and its container()
/// as the parent. Queued, it holds a ref on R's count until the run's done.
/// The path never leaves what the parent is in, and a ref on R's count
/// keeps all it's in: so the parent outlives the run.
///
/// C has it by address - on the workqueue's lists while it's pending, no
/// borrow held - so it's never copied, cloned or moved: it's not Copy or
/// Clone, and !Unpin, checked below. A mem::swap() of two through &mut
/// parents would still move one: place.rs's known gap, Fs never had by &mut.
#[repr(C)]
pub(crate) struct ReffedWork<F: WorkFunc, R> {
    work:       UnsafeCell<c::work_struct>,
    _placed_at: F::At,
    _r:         PhantomData<fn() -> R>,
    /// !Unpin - a PhantomData of it, as At's, for improper_ctypes
    _pin:       PhantomData<PhantomPinned>,
}

// Not Copy, Clone or Unpin: an impl of either trait makes check()'s call
// resolve to two impls of NotImpl, and fail to compile.
const _: () = {
    trait NotImpl<A> {
        fn check() {}
    }
    impl<T: ?Sized> NotImpl<()> for T {}
    struct IsClone;
    struct IsUnpin;
    impl<T: ?Sized + Clone> NotImpl<IsClone> for T {}
    impl<T: ?Sized + Unpin> NotImpl<IsUnpin> for T {}

    #[allow(dead_code)]
    fn check<F: WorkFunc, R>() {
        let _ = <ReffedWork<F, R> as NotImpl<_>>::check;
    }
};

// SAFETY: the workqueue's to synchronize, from any thread
unsafe impl<F: WorkFunc, R> Send for ReffedWork<F, R> {}
unsafe impl<F: WorkFunc, R> Sync for ReffedWork<F, R> {}

// SAFETY: its At field, given by the parent's #[derive(Init)], from at(),
// for the field it names
unsafe impl<F: WorkFunc, R> Placed for ReffedWork<F, R>
where
    F::At: Place<Type = Self>,
{
    type Parent = Parent<F>;
    type At = F::At;

    unsafe fn placed_at_raw(p: *const Self) -> *const F::At {
        // SAFETY: the caller's
        unsafe { &raw const (*p)._placed_at }
    }
}

impl<F: WorkFunc, R: Path<Parent<F>>> ReffedWork<F, R>
where
    F::At: Place<Type = Self>,
    R::To: TryRefcount,
{
    /// Run F on @wq, holding a ref on R's count: false if there's none to
    /// be had - tryget_live(). Queued already, the pending run's ref is the
    /// one there'll be.
    pub(crate) fn queue(&self, wq: &Queue) -> bool {
        // taken first: the run can start, and put it, as soon as it's queued
        let Some(r) = R::get(self.container()).tryget_live() else { return false };
        // SAFETY: made in place, by new(); the ref keeps it, for the run
        if unsafe { wq.queue_work(self.work.get()) } {
            // the run's, which adopts it
            r.leak();
        }
        // else put: the pending run has its own
        true
    }

    /// # Safety
    /// @work is a ReffedWork<F, R>'s, queued by queue(), holding its ref.
    unsafe extern "C" fn work_fn(work: *mut c::work_struct) {
        // SAFETY: only a ReffedWork<F, R> runs its work_fn - its work_struct
        // first - and the ref it holds keeps its parent
        let p = unsafe { (*work.cast::<Self>()).container() };
        // SAFETY: the ref queue() took for this run: put as it ends, its last
        // touch of the parent
        let _r = unsafe { RefGuard::adopt(R::get(p)) };
        F::run(p);
    }

    /// INIT_WORK(), with a lockdep class of its own - @key, named @name, a
    /// static per field in the parent's #[derive(Init)].
    #[cfg(kernel)]
    #[doc(hidden)]
    pub(crate) fn new<E>(
        at:   F::At,
        name: &'static core::ffi::CStr,
        key:  core::pin::Pin<&'static kernel::sync::LockClassKey>,
    ) -> impl ::pin_init::PinInit<Self, E> {
        // SAFETY: the work_struct and the At, all of it, written in place
        unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut Self| {
                c::init_work_with_key(slot.cast(), Some(Self::work_fn), false, name.as_ptr().cast(),
                                      key.as_ptr());
                (&raw mut (*slot)._placed_at).write(at);
                Ok(())
            })
        }
    }

    /// INIT_WORK(), as the shim has it: no lockdep.
    #[cfg(not(kernel))]
    #[doc(hidden)]
    pub(crate) fn new<E>(at: F::At) -> impl ::pin_init::PinInit<Self, E> {
        // SAFETY: the work_struct and the At, all of it, written in place
        unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut Self| {
                c::bch2_init_work(slot.cast(), Some(Self::work_fn));
                (&raw mut (*slot)._placed_at).write(at);
                Ok(())
            })
        }
    }
}

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
        unsafe { c::cancel_delayed_work_sync(self.as_ptr()) }
    }
}
