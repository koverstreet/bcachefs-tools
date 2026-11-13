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
//! thread, a daemon: the reconcile thread, copygc, journal reclaim - each a
//! Daemon, which holds it from start to stop for whoever kicks it. Jobs
//! fanned out and joined are util::par's.
//!
//! The thread's closure is in an allocation the Kthread owns, freed once the
//! thread's stopped - not the thread's to free: as the kernel port's, though
//! here a thread stopped before it first ran does call its function, so the
//! closure isn't run once the thread's been told to stop.
//!
//! A daemon is kicked when there's work for it - kick(): a count bumped, then
//! a wake. Its thread, handed its side (KthreadSelf), notes the count before a
//! pass and sleeps after it until the count's moved - or a timer, or a stop -
//! and whatever it's sleeping in, the wake has it recheck. C's daemons kept the
//! count themselves, next to an RCU pointer to the thread to wake.
//!
//! The kthreads themselves are below: a std thread with the shim's
//! task_struct, created stopped - in schedule() till woken - to run its
//! function, then exit, which kthread_stop() joins. C's kthread_create(),
//! kthread_stop() and kthread_should_stop() are them too, for the C still
//! calling them.

use super::sched::{get_current, wake_up_process};
use crate::c;
use crate::errcode::{errptr_to_result, BchError};
use crate::locking::Locked;
use crate::util::alloc::{kbox_pin_init, AllocError, KBox};
use crate::util::rcu::{FreeAnytime, NothingToDetach};
use crate::util::Printbuf;
use core::cell::UnsafeCell;
use core::ffi::{c_char, c_int, c_void, CStr};
use core::fmt;
use core::mem::{align_of, needs_drop, size_of, ManuallyDrop};
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering, Ordering::SeqCst};
use pin_init::{pin_data, pin_init, PinInit};
use std::thread::JoinHandle;

unsafe extern "C" fn run_raw<F: FnOnce()>(data: *mut c_void) -> c_int {
    // SAFETY: create() wrote an F here
    let f = unsafe { (&raw const data).cast::<F>().read() };
    f();
    0
}

/// A ref on a kthread's task_struct: to look at it - its backtrace - from
/// another thread, after it's exited too.
pub struct KthreadTask(NonNull<c::task_struct>);

unsafe impl Send for KthreadTask {}
unsafe impl Sync for KthreadTask {}

impl KthreadTask {
    pub fn as_ptr(&self) -> *mut c::task_struct {
        self.0.as_ptr()
    }
}

impl Clone for KthreadTask {
    fn clone(&self) -> Self {
        unsafe { c::get_task_struct(self.0.as_ptr()) };
        Self(self.0)
    }
}

impl Drop for KthreadTask {
    fn drop(&mut self) {
        unsafe { c::put_task_struct(self.0.as_ptr()) };
    }
}

/// A kthread, by a ref on its task_struct.
pub struct KthreadRef(KthreadTask);

impl KthreadRef {
    /// Created stopped, to run @f once it's woken - or once it's stopped, if
    /// it's stopped first: @f checks Kthread::should_stop().
    ///
    /// # Safety
    /// What @f uses lives until it returns, or the thread's stopped.
    pub unsafe fn create<F: FnOnce() + Send>(name: fmt::Arguments<'_>, f: F) -> Result<Self, BchError> {
        // the data pointer, as the kernel port's: there, a thread stopped
        // before it ran never calls @f, so it mustn't need dropping
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
            kthread_create_name(Some(run_raw::<F>), data, name_buf.as_c_str().as_ptr().cast())
        };
        let task = errptr_to_result(NonNull::new(task).expect("kthread_create() gives a task or an error"))?;
        // so it can be stopped after it's exited
        unsafe { c::get_task_struct(task.as_ptr()) };
        Ok(Self(KthreadTask(task)))
    }

    /// Start it, or wake it from a sleep: wake_up_process().
    pub fn wake(&self) {
        unsafe { wake_up_process(self.0.as_ptr()) };
    }

    /// Tell it to stop, and wait for it to: never from the thread itself. It
    /// may have returned already, or never have run.
    pub fn stop(self) {
        unsafe { kthread_stop(self.0.as_ptr()) };
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
    thread: KBox<Thread<dyn Send + FreeAnytime + NothingToDetach>>,
}

impl Kthread {
    /// Thread @name, to run @f once woken - created stopped, as
    /// kthread_create().
    pub fn create<F: FnOnce(&KthreadSelf<'_>) + Send + FreeAnytime + NothingToDetach + 'static>(name: fmt::Arguments<'_>, f: F)
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

    pub fn task(&self) -> KthreadTask {
        self.task.0.clone()
    }

    /// The kicks it's had, for messages.
    pub fn kicks(&self) -> u32 {
        self.thread.kicks.load(Ordering::Relaxed)
    }

    /// Whether the current thread is a kthread told to stop:
    /// kthread_should_stop() - false on a thread that isn't one.
    pub fn should_stop() -> bool {
        let flags = unsafe { (*get_current()).flags };
        flags & c::PF_KTHREAD != 0 && kthread_should_stop()
    }

    /// The kernel port's: there's no freezer here.
    pub fn set_freezable() {}

    pub fn freezing() -> bool {
        false
    }

    pub fn try_to_freeze() -> bool {
        false
    }
}

impl Drop for Kthread {
    fn drop(&mut self) {
        // SAFETY: taken once, here; @thread's dropped after
        unsafe { ManuallyDrop::take(&mut self.task) }.stop();
    }
}

/// A daemon's thread, from its start to its stop: kicked, woken and looked at
/// under its lock, started and stopped outside it - kthread_stop() sleeps.
#[pin_data]
pub struct Daemon {
    #[pin]
    thread: Locked<c::spinlock_t, Option<Kthread>>,
}

impl Daemon {
    pub fn new() -> impl PinInit<Self> {
        pin_init!(Self { thread <- crate::new_lock!(None, "daemon thread") })
    }

    /// Started, and not yet stopped - though it may have returned.
    pub fn started(&self) -> bool {
        self.thread.lock().is_some()
    }

    /// Start it, unless it's started: Kthread::create(), then woken.
    pub fn start<F: FnOnce(&KthreadSelf<'_>) + Send + FreeAnytime + NothingToDetach + 'static>(
        &self, name: fmt::Arguments<'_>, f: F) -> Result<(), BchError>
    {
        if self.started() {
            return Ok(());
        }

        let t = Kthread::create(name, f)?;
        // another start got there first: ours is stopped outside the lock
        let lost = {
            let mut slot = self.thread.lock();
            if slot.is_some() {
                Some(t)
            } else {
                slot.insert(t).wake();
                None
            }
        };
        drop(lost);
        Ok(())
    }

    /// Stop it, and wait for it to.
    pub fn stop(&self) {
        let t = self.thread.lock().take();
        drop(t);
    }

    pub fn kick(&self) {
        if let Some(t) = &*self.thread.lock() {
            t.kick();
        }
    }

    pub fn wake(&self) {
        if let Some(t) = &*self.thread.lock() {
            t.wake();
        }
    }

    pub fn kicks(&self) -> Option<u32> {
        self.thread.lock().as_ref().map(Kthread::kicks)
    }

    /// Its task, for a backtrace: printed outside the lock, as that allocates.
    pub fn task(&self) -> Option<KthreadTask> {
        self.thread.lock().as_ref().map(Kthread::task)
    }
}

// ── The kthreads: a std thread and the shim's task_struct ──────────────────

const KTHREAD_SHOULD_STOP: u32 = 1;

unsafe extern "C" {
    fn calloc(n: usize, size: usize) -> *mut c_void;
}

/// What a thread's tested and set atomically: kthread_flags.
fn kthread_flags<'a>(p: *mut c::task_struct) -> &'a AtomicUsize {
    unsafe { AtomicUsize::from_ptr((&raw mut (*p).kthread_flags).cast()) }
}

/// A kthread's task_struct, and its thread: joined by kthread_stop(), or
/// detached when the task's freed.
#[repr(C)]
struct KthreadStruct {
    task:   c::task_struct,
    thread: Option<JoinHandle<()>>,
}

fn to_kthread(p: *mut c::task_struct) -> *mut KthreadStruct {
    p.cast()
}

/// @p's last ref is put: a kthread's thread, unless it's been joined, is
/// detached.
pub(super) unsafe fn task_free(p: *mut c::task_struct) {
    unsafe {
        if (*p).flags & c::PF_KTHREAD != 0 {
            drop((*to_kthread(p)).thread.take());
        }
    }
}

struct Task(*mut c::task_struct);

unsafe impl Send for Task {}

fn kthread_start_fn(p: Task) {
    let p = p.0;
    unsafe {
        super::sched::set_current(p);
        c::linux_thread_init();
        c::schedule();
        (*p).thread_fn.expect("a kthread's function")((*p).thread_data);

        c::put_task_struct(p);
        c::linux_thread_exit();
    }
}

/// kthread_create(), the name formatted: created stopped - wake_up_process()
/// starts it. Its task_struct, or ERR_PTR(-errno).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kthread_create_name(thread_fn: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
                                             thread_data: *mut c_void,
                                             name: *const c_char) -> *mut c::task_struct {
    let k = unsafe { calloc(1, size_of::<KthreadStruct>()) }.cast::<KthreadStruct>();
    if k.is_null() {
        return (-(c::ENOMEM as isize)) as *mut c::task_struct;
    }
    let p = unsafe { &raw mut (*k).task };

    let name = unsafe { CStr::from_ptr(name) };
    unsafe {
        (&raw mut (*k).thread).write(None);

        let comm = &mut (*p).comm;
        let len = comm.len() - 1;
        for (d, &s) in comm[..len].iter_mut().zip(name.to_bytes()) {
            *d = s as c_char;
        }

        (*p).flags       |= c::PF_KTHREAD;
        (*p).thread_fn    = thread_fn;
        (*p).thread_data  = thread_data;
        (*p).state        = c::TASK_UNINTERRUPTIBLE as c_int;
        (*p).signal       = &raw mut (*p)._signal;
        (*p).usage.counter = 1;
        c::init_rwsem(&raw mut (*p)._signal.exec_update_lock);
    }

    let mut ret = 0;
    for _ in 0..10 {
        let task = Task(p);
        match std::thread::Builder::new()
            .name(name.to_string_lossy().into_owned())
            .stack_size(256 << 10)
            .spawn(move || kthread_start_fn(task))
        {
            Ok(t) => {
                unsafe { (*k).thread = Some(t) };
                return p;
            }
            Err(e) => ret = e.raw_os_error().unwrap_or(c::ENOMEM as i32),
        }

        super::shrinker::run_shrinkers(c::GFP_KERNEL, true);
    }
    (-(ret as isize)) as *mut c::task_struct
}

/// Whether this kthread should return now: someone's called kthread_stop()
/// on it, which waits for it to.
#[unsafe(no_mangle)]
pub extern "C" fn kthread_should_stop() -> bool {
    let p = get_current();
    kthread_flags(p).load(SeqCst) & (1 << KTHREAD_SHOULD_STOP) != 0
}

/// Set kthread_should_stop() for @p, wake it, and wait for it to exit. It
/// may never have been woken: it runs its function, then exits.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kthread_stop(p: *mut c::task_struct) -> c_int {
    unsafe {
        c::get_task_struct(p);

        kthread_flags(p).fetch_or(1 << KTHREAD_SHOULD_STOP, SeqCst);
        wake_up_process(p);
        if let Some(thread) = (*to_kthread(p)).thread.take() {
            let _ = thread.join();
        }

        c::put_task_struct(p);
    }
    0
}
