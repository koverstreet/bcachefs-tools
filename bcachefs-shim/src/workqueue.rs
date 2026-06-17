// SPDX-License-Identifier: GPL-2.0

//! Userspace kernel-compat workqueue wrapper.
//!
//! This mirrors the closure-spawn part of `kernel::workqueue` for code that
//! also needs to run against the userspace shim.

use crate::c;
use core::cell::UnsafeCell;

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

pub fn system_unbound() -> &'static Queue {
    unsafe { Queue::from_raw(c::system_unbound_wq) }
}
