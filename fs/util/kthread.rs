// SPDX-License-Identifier: GPL-2.0

//! A kthread Rust owns: made stopped, woken, stopped - kthread_create(),
//! wake_up_process(), kthread_stop() - for a long-lived thread, a daemon: the
//! reconcile thread, copygc, journal reclaim. Jobs fanned out and joined are
//! util::par's.
//!
//! The thread's closure is in an allocation the Kthread owns, freed once the
//! thread's stopped - not the thread's to free: stopped before it first ran,
//! the kernel's kthread() never calls its function. Userspace's does, so the
//! closure isn't run once the thread's been told to stop, in either build.
//!
//! One implementation for both builds, over C's API - userspace's is
//! linux/kthread.c - through util.c's wrappers.

use crate::c;
use crate::errcode::{errptr_to_result, BchError};
use crate::util::alloc::{alloc_pinned, free_pinned, AllocError};
use crate::util::Printbuf;
use core::cell::UnsafeCell;
use core::ffi::{c_int, c_void};
use core::fmt;
use core::ptr::NonNull;

/// What the thread runs, until it takes it.
struct Thread<F> {
    f: UnsafeCell<Option<F>>,
}

pub struct Kthread {
    task:   NonNull<c::task_struct>,
    thread: *mut c_void,
    free:   unsafe fn(*mut c_void),
}

// SAFETY: a task ref, which any thread may wake, stop and put; the closure,
// Send, touched by its thread until it's stopped and by drop() after
unsafe impl Send for Kthread {}
unsafe impl Sync for Kthread {}

impl Kthread {
    /// Thread @name, to run @f once woken - created stopped, as
    /// kthread_create().
    pub fn create<F: FnOnce() + Send + 'static>(name: fmt::Arguments<'_>, f: F) -> Result<Self, BchError> {
        let thread = alloc_pinned::<Thread<F>, AllocError>(Ok(Thread { f: UnsafeCell::new(Some(f)) }))?;

        let mut name_buf = Printbuf::new();
        write!(name_buf, "{name}");

        // SAFETY: @thread is run::<F>'s, until we free it after stopping it
        let task = unsafe {
            c::bch2_kthread_create(Some(run::<F>), thread.cast(), name_buf.as_c_str().as_ptr().cast())
        };
        let task = match errptr_to_result(NonNull::new(task).expect("kthread_create() gives a task or an error")) {
            Ok(task) => task,
            Err(e) => {
                // SAFETY: the thread wasn't made, so nothing else has it
                unsafe { free_pinned(thread) };
                return Err(e);
            }
        };

        // Ours, so it can be stopped after it's exited
        // SAFETY: a task just made
        unsafe { c::get_task_struct(task.as_ptr()) };
        Ok(Kthread { task, thread: thread.cast(), free: free_thread::<F> })
    }

    /// Start it, or wake it from a sleep: wake_up_process().
    pub fn wake(&self) {
        // SAFETY: our task ref
        unsafe { c::wake_up_process(self.task.as_ptr()) };
    }

    /// The task, for C that wakes it by pointer.
    pub fn as_ptr(&self) -> *mut c::task_struct {
        self.task.as_ptr()
    }

    /// Whether the current thread is a kthread told to stop:
    /// kthread_should_stop() - false on a thread that isn't one.
    pub fn should_stop() -> bool {
        // SAFETY: plain; it checks PF_KTHREAD first
        unsafe { c::bch2_kthread_should_stop() }
    }

    /// Tell it to stop, and wait for it to: kthread_stop(). It may have
    /// returned already, or never have run.
    pub fn stop(self) {}
}

impl Drop for Kthread {
    fn drop(&mut self) {
        // SAFETY: our task ref, put once it's exited; then nothing runs its
        // closure, so it's ours to free
        unsafe {
            c::bch2_kthread_stop(self.task.as_ptr());
            c::put_task_struct(self.task.as_ptr());
            (self.free)(self.thread);
        }
    }
}

/// The thread's function: @f, unless it's been told to stop before it ran.
unsafe extern "C" fn run<F: FnOnce() + Send + 'static>(data: *mut c_void) -> c_int {
    if Kthread::should_stop() {
        return 0;
    }
    // SAFETY: the Kthread's, which frees it only once we've exited; ours to
    // take from, as nothing else touches it while we run
    let f = unsafe { (*(*data.cast::<Thread<F>>()).f.get()).take() };
    if let Some(f) = f {
        f();
    }
    0
}

/// # Safety
/// @p is a Thread<F> from Kthread::create(), its thread exited.
unsafe fn free_thread<F>(p: *mut c_void) {
    // SAFETY: the caller's
    unsafe { free_pinned(p.cast::<Thread<F>>()) };
}
