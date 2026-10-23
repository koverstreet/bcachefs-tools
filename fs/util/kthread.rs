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
//! A daemon is kicked when there's work for it - kick(): a count bumped, then
//! a wake. Its thread, handed its side (KthreadSelf), notes the count before a
//! pass and sleeps after it until the count's moved - or a timer, or a stop -
//! and whatever it's sleeping in, the wake has it recheck. C's daemons kept the
//! count themselves, next to an RCU pointer to the thread to wake.
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
use core::sync::atomic::{AtomicU32, Ordering};

/// What the thread runs, until it takes it; and its kicks, which outlive it.
struct Thread<F> {
    kicks: AtomicU32,
    f:     UnsafeCell<Option<F>>,
}

pub struct Kthread {
    task:   NonNull<c::task_struct>,
    thread: *mut c_void,
    /// The Thread's, which lives as long as we do
    kicks:  NonNull<AtomicU32>,
    free:   unsafe fn(*mut c_void),
}

/// The thread's side of its Kthread, handed to its closure: the kicks it's
/// had - the count Kthread::kick() bumps - to compare against one it saw.
pub struct KthreadSelf<'a> {
    kicks: &'a AtomicU32,
}

impl KthreadSelf<'_> {
    /// The kicks so far: what a later kicked_since() compares against.
    pub fn kicks(&self) -> u32 {
        self.kicks.load(Ordering::Acquire)
    }

    /// Whether it's been kicked since it saw @seen kicks.
    pub fn kicked_since(&self, seen: u32) -> bool {
        self.kicks() != seen
    }
}

// SAFETY: a task ref, which any thread may wake, stop and put; the closure,
// Send, touched by its thread until it's stopped and by drop() after; the
// kicks, an atomic
unsafe impl Send for Kthread {}
unsafe impl Sync for Kthread {}

impl Kthread {
    /// Thread @name, to run @f once woken - created stopped, as
    /// kthread_create().
    pub fn create<F: FnOnce(&KthreadSelf<'_>) + Send + 'static>(name: fmt::Arguments<'_>, f: F)
        -> Result<Self, BchError>
    {
        let thread = alloc_pinned::<Thread<F>, AllocError>(Ok(Thread {
            kicks: AtomicU32::new(0),
            f:     UnsafeCell::new(Some(f)),
        }))?;

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
        // SAFETY: the allocation's, which lives until drop() frees it
        let kicks = unsafe { NonNull::from(&(*thread).kicks) };
        Ok(Kthread { task, thread: thread.cast(), kicks, free: free_thread::<F> })
    }

    /// Start it, or wake it from a sleep: wake_up_process().
    pub fn wake(&self) {
        // SAFETY: our task ref
        unsafe { c::wake_up_process(self.task.as_ptr()) };
    }

    /// Kick it: its kicks bumped, then woken from whatever it's sleeping in.
    /// A sleep that's for a kick - KthreadSelf::kicked_since() in its
    /// condition - ends; any other wakes, rechecks its own, and sleeps on.
    /// The bump comes first, so a sleep checking after it sees it, and one
    /// that checked before is woken.
    pub fn kick(&self) {
        // SAFETY: the Thread's, which lives as long as we do
        unsafe { self.kicks.as_ref() }.fetch_add(1, Ordering::Release);
        self.wake();
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

    /// Make the current kthread one the freezer stops, as set_freezable():
    /// it's frozen in place in a freezable sleep (Sleep::freezable()), so
    /// every long sleep it does must be one, or it holds up suspend.
    /// Userspace has no freezer.
    pub fn set_freezable() {
        // SAFETY: of the current task
        #[cfg(kernel)]
        let _ = unsafe { c::set_freezable() };
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
unsafe extern "C" fn run<F: FnOnce(&KthreadSelf<'_>) + Send + 'static>(data: *mut c_void) -> c_int {
    if Kthread::should_stop() {
        return 0;
    }
    // SAFETY: the Kthread's, which frees it only once we've exited; ours to
    // take from, as nothing else touches it while we run
    let thread = unsafe { &*data.cast::<Thread<F>>() };
    let f = unsafe { (*thread.f.get()).take() };
    if let Some(f) = f {
        f(&KthreadSelf { kicks: &thread.kicks });
    }
    0
}

/// # Safety
/// @p is a Thread<F> from Kthread::create(), its thread exited.
unsafe fn free_thread<F>(p: *mut c_void) {
    // SAFETY: the caller's
    unsafe { free_pinned(p.cast::<Thread<F>>()) };
}
