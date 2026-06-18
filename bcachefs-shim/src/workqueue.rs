// SPDX-License-Identifier: GPL-2.0

//! Userspace kernel-compat workqueue wrapper.
//!
//! This mirrors the closure-spawn part of `kernel::workqueue` for code that
//! also needs to run against the userspace shim.

use crate::c;
use core::cell::UnsafeCell;
use core::marker::PhantomData;
use std::sync::Arc;

pub type AllocFlags = c::gfp_t;

pub mod flags {
    use super::{c, AllocFlags};

    pub const GFP_KERNEL: AllocFlags = c::GFP_KERNEL;
}

#[derive(Copy, Clone, Debug)]
pub struct AllocError;

#[repr(transparent)]
pub struct Queue(UnsafeCell<c::workqueue_struct>);

// SAFETY: C workqueue operations serialize their own internal state.
unsafe impl Send for Queue {}
// SAFETY: C workqueue operations serialize their own internal state.
unsafe impl Sync for Queue {}

impl Queue {
    /// Wrap a raw C workqueue pointer.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a valid workqueue that outlives the returned
    /// reference.
    pub unsafe fn from_raw<'a>(ptr: *mut c::workqueue_struct) -> &'a Queue {
        &*ptr.cast::<Queue>()
    }

    pub fn try_spawn<T>(&self, _flags: AllocFlags, func: T) -> Result<(), AllocError>
    where
        T: 'static + Send + FnOnce(),
    {
        let mut raw_work = c::work_struct::default();
        raw_work.data.counter = 0;
        raw_work.func = Some(closure_work_fn::<T>);

        let work = Box::new(ClosureWork {
            work: raw_work,
            func: Some(func),
        });
        let work = Box::into_raw(work);

        // INIT_WORK initializes the list head to point to itself.
        unsafe {
            (*work).work.entry.next = &mut (*work).work.entry;
            (*work).work.entry.prev = &mut (*work).work.entry;
        }

        let queued = unsafe {
            c::queue_work(self.0.get(), &mut (*work).work)
        };
        if queued {
            Ok(())
        } else {
            unsafe { drop(Box::from_raw(work)); }
            Err(AllocError)
        }
    }

    /// Enqueue a persistent work item — a task that owns an embedded [`Work`].
    ///
    /// Re-enqueueing while the item is still pending is a no-op (the C
    /// `work_struct`'s pending bit dedups), which is exactly what lets an
    /// executor re-poll a task on wake without a fresh allocation.
    pub fn enqueue<W: WorkItem>(&self, item: Arc<W>) {
        // Leak one reference to the work queue; the trampoline reclaims it.
        let ptr = Arc::into_raw(item);

        // SAFETY: `WORK_OFFSET` locates the embedded `Work<W>` (a transparent
        // wrapper over `work_struct`) inside `*ptr`.
        let work = unsafe { (ptr as *const u8).add(W::WORK_OFFSET) as *mut c::work_struct };

        unsafe {
            // Lazy one-time init. The first enqueue happens from the spawner,
            // before the task is shared, so this is not racy; later enqueues
            // (wakes) see `func` already set and skip it.
            if (*work).func.is_none() {
                (*work).entry.next = &mut (*work).entry;
                (*work).entry.prev = &mut (*work).entry;
                (*work).func = Some(run_work_fn::<W>);
            }

            if !c::queue_work(self.0.get(), work) {
                // Already pending: drop the reference we just leaked.
                drop(Arc::from_raw(ptr));
            }
        }
    }
}

struct ClosureWork<T> {
    work: c::work_struct,
    func: Option<T>,
}

unsafe extern "C" fn closure_work_fn<T>(work: *mut c::work_struct)
where
    T: 'static + Send + FnOnce(),
{
    let work = work.cast::<ClosureWork<T>>();
    let mut work = unsafe { Box::from_raw(work) };

    if let Some(func) = work.func.take() {
        func();
    }
}

/// An embedded `work_struct`, tagged with its containing task type. Mirrors
/// `kernel::workqueue::Work<T>`. Construct with [`Work::new`]; it is initialised
/// lazily on the first [`Queue::enqueue`], which is fine because by then it
/// lives at its final address inside an `Arc`.
#[repr(transparent)]
pub struct Work<T: ?Sized> {
    work: UnsafeCell<c::work_struct>,
    _owner: PhantomData<fn(T)>,
}

// SAFETY: the inner `work_struct` is only touched via `Queue::enqueue` (which
// the C workqueue serializes) and the owning task; the handle is thread-safe.
unsafe impl<T: ?Sized> Send for Work<T> {}
unsafe impl<T: ?Sized> Sync for Work<T> {}

impl<T: ?Sized> Work<T> {
    pub fn new() -> Self {
        let mut work = c::work_struct::default();
        work.data.counter = 0;
        // `func` stays null; `Queue::enqueue` sets it on first use, once the
        // concrete `WorkItem` and the work's final address are known.
        Work {
            work: UnsafeCell::new(work),
            _owner: PhantomData,
        }
    }
}

impl<T: ?Sized> Default for Work<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// A task that can be re-run on a [`Queue`]. Mirrors `kernel::workqueue::WorkItem`.
///
/// # Safety
///
/// `WORK_OFFSET` must be the byte offset of an embedded `Work<Self>` field
/// within `Self`, so [`Queue::enqueue`]'s trampoline can recover `Arc<Self>`
/// from the `work_struct` pointer.
pub unsafe trait WorkItem: Send + Sync + Sized + 'static {
    /// Offset of the embedded `Work<Self>` field within `Self`.
    const WORK_OFFSET: usize;

    /// Run the work item, on a workqueue thread.
    fn run(self: Arc<Self>);
}

/// C trampoline: recover the `Arc<W>` that `enqueue` leaked and run it.
unsafe extern "C" fn run_work_fn<W: WorkItem>(work: *mut c::work_struct) {
    // SAFETY: `work` points at the embedded `Work<W>` field; back out to the
    // containing `W` — exactly the pointer `enqueue` produced via `into_raw`.
    let ptr = unsafe { (work as *const u8).sub(W::WORK_OFFSET) as *const W };
    let item = unsafe { Arc::from_raw(ptr) };
    W::run(item);
}

pub fn system_unbound() -> &'static Queue {
    unsafe { Queue::from_raw(c::system_unbound_wq) }
}
