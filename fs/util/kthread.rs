// SPDX-License-Identifier: GPL-2.0

//! KthreadRef is a kthread by a ref on its task_struct: made stopped, woken,
//! stopped - kthread_create(), wake_up_process(), kthread_stop() - to run a
//! closure. The closure is the thread's data pointer, by value: no bigger
//! than one, and nothing to drop - what it uses, its creator keeps for it.
//! So its trampoline, run_raw::<F>(), is one function, the closure inlined
//! into it. Its drop only puts the ref, as a thread that exits by itself - a
//! task's, util::task - may be the one dropping it.
//!
//! Kthread is a kthread Rust owns, stopped by its drop: for a long-lived
//! thread, a daemon: the reconcile thread, copygc, journal reclaim. Jobs
//! fanned out and joined are util::par's.
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
use crate::util::alloc::{kbox_pin_init, AllocError, KBox};
use crate::util::Printbuf;
use core::cell::UnsafeCell;
use core::ffi::{c_int, c_void};
use core::fmt;
use core::mem::{align_of, needs_drop, size_of, ManuallyDrop};
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering};

unsafe extern "C" fn run_raw<F: FnOnce()>(data: *mut c_void) -> c_int {
    // SAFETY: create() wrote an F here
    let f = unsafe { (&raw const data).cast::<F>().read() };
    f();
    0
}

/// A kthread, by a ref on its task_struct.
pub struct KthreadRef(NonNull<c::task_struct>);

// SAFETY: a ref any thread may put, on a thread any thread may wake or stop
unsafe impl Send for KthreadRef {}
unsafe impl Sync for KthreadRef {}

impl KthreadRef {
    /// Created stopped, to run @f once it's woken - or, in userspace, once
    /// it's stopped, if it's stopped first: @f checks Kthread::should_stop().
    ///
    /// # Safety
    /// What @f uses lives until it returns, or the thread's stopped.
    pub unsafe fn create<F: FnOnce() + Send>(name: fmt::Arguments<'_>, f: F) -> Result<Self, BchError> {
        // the data pointer: in-kernel, a thread stopped before it ran never
        // calls @f, so it mustn't need dropping
        const {
            assert!(size_of::<F>() <= size_of::<*mut c_void>()
                    && align_of::<F>() <= align_of::<*mut c_void>()
                    && !needs_drop::<F>())
        };
        let mut data = core::ptr::null_mut::<c_void>();
        // SAFETY: it fits
        unsafe { (&raw mut data).cast::<F>().write(f) };

        let mut name_buf = Printbuf::new();
        write!(name_buf, "{name}");

        let task = unsafe {
            c::bch2_kthread_create(Some(run_raw::<F>), data, name_buf.as_c_str().as_ptr().cast())
        };
        let task = errptr_to_result(NonNull::new(task).expect("kthread_create() gives a task or an error"))?;
        // so it can be stopped after it's exited
        unsafe { c::get_task_struct(task.as_ptr()) };
        Ok(Self(task))
    }

    /// Start it, or wake it from a sleep: wake_up_process().
    pub fn wake(&self) {
        unsafe { c::wake_up_process(self.0.as_ptr()) };
    }

    /// The task, for C that wakes it by pointer.
    pub fn as_ptr(&self) -> *mut c::task_struct {
        self.0.as_ptr()
    }

    /// Tell it to stop, and wait for it to: never from the thread itself. It
    /// may have returned already, or never have run.
    pub fn stop(self) {
        unsafe { c::bch2_kthread_stop(self.0.as_ptr()) };
    }
}

impl Drop for KthreadRef {
    fn drop(&mut self) {
        unsafe { c::put_task_struct(self.0.as_ptr()) };
    }
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

/// What the thread runs, until it takes it - an Option<F>, erased in the
/// Kthread - and its kicks, which outlive it.
struct Thread<T: ?Sized> {
    kicks: AtomicU32,
    f:     UnsafeCell<T>,
}

// SAFETY: @f is touched by the thread, once, and otherwise only by its drop,
// after the thread's stopped
unsafe impl<T: ?Sized + Send> Sync for Thread<T> {}

pub struct Kthread {
    task:   ManuallyDrop<KthreadRef>,
    /// Dropped after the stop, and never moved out of: the thread holds a
    /// reference into it. Not a Pin<KBox>: the kernel's Box coerces to the
    /// dyn, but not in a Pin - it has no PinCoerceUnsized
    thread: KBox<Thread<dyn Send>>,
}

impl Kthread {
    /// Thread @name, to run @f once woken - created stopped, as
    /// kthread_create().
    pub fn create<F: FnOnce(&KthreadSelf<'_>) + Send + 'static>(name: fmt::Arguments<'_>, f: F)
        -> Result<Self, BchError>
    {
        let thread = kbox_pin_init::<_, AllocError>(Ok(Thread {
            kicks: AtomicU32::new(0),
            f:     UnsafeCell::new(Some(f)),
        }))?;
        // SAFETY: in a KBox the Kthread never moves it out of
        let thread = unsafe { Pin::into_inner_unchecked(thread) };
        let t: &Thread<Option<F>> = &thread;
        let run = move || {
            if !Kthread::should_stop() {
                // SAFETY: the thread's, once
                let f = unsafe { (*t.f.get()).take() }.expect("a kthread's closure run twice");
                f(&KthreadSelf { kicks: &t.kicks });
            }
        };
        // SAFETY: @thread outlives it: it's dropped after the stop
        let task = unsafe { KthreadRef::create(name, run) }?;
        Ok(Kthread { task: ManuallyDrop::new(task), thread })
    }

    /// Start it, or wake it from a sleep: wake_up_process().
    pub fn wake(&self) {
        self.task.wake();
    }

    /// Kick it: its kicks bumped, then woken from whatever it's sleeping in.
    /// A sleep that's for a kick - KthreadSelf::kicked_since() in its
    /// condition - ends; any other wakes, rechecks its own, and sleeps on.
    /// The bump comes first, so a sleep checking after it sees it, and one
    /// that checked before is woken.
    pub fn kick(&self) {
        self.thread.kicks.fetch_add(1, Ordering::Release);
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

    /// Whether the freezer wants the current thread: freezing(current). A
    /// freezable kthread that can go a long while without a freezable sleep
    /// - copygc, when there's always more to move - checks between pieces of
    /// work, and stops to try_to_freeze().
    pub fn freezing() -> bool {
        // SAFETY: of the current task
        #[cfg(kernel)]
        let f = unsafe { c::freezing(c::bch2_current()) };
        #[cfg(not(kernel))]
        let f = false;
        f
    }

    /// Freeze here, if the freezer wants us, until thawed: try_to_freeze().
    /// Whether it froze. With no locks held - they'd stay held till the thaw.
    pub fn try_to_freeze() -> bool {
        // SAFETY: of the current task
        #[cfg(kernel)]
        let f = unsafe { c::try_to_freeze() };
        #[cfg(not(kernel))]
        let f = false;
        f
    }
}

impl Drop for Kthread {
    fn drop(&mut self) {
        // SAFETY: taken once, here; @thread's dropped after
        unsafe { ManuallyDrop::take(&mut self.task) }.stop();
    }
}
