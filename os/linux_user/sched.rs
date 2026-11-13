// SPDX-License-Identifier: GPL-2.0

//! The shim's scheduler: each thread's task_struct, current, and sleeping
//! and waking on its state - a futex word.

use crate::c;
use crate::os::kernel::{jiffies, HZ, NSEC_PER_SEC};
use core::cell::Cell;
use core::ffi::{c_int, c_long, c_ulong, c_void};
use core::ptr::null_mut;
use core::sync::atomic::{fence, AtomicU32, Ordering::{Acquire, Relaxed, SeqCst}};
use rustix::thread::futex;
use std::time::{Duration, Instant};

unsafe extern "C" {
    fn calloc(n: usize, size: usize) -> *mut c_void;
}

const TASK_RUNNING: u32 = c::TASK_RUNNING;

thread_local! {
    static CURRENT: Cell<*mut c::task_struct> = const { Cell::new(null_mut()) };
}

/// The task state, the futex word a sleep waits on.
fn state<'a>(p: *mut c::task_struct) -> &'a AtomicU32 {
    unsafe { AtomicU32::from_ptr((&raw mut (*p).state).cast()) }
}

/// A task for the thread that calls this - not one of kthread_create()'s,
/// which makes its own: the main thread's, a test harness's, fuse's workers.
fn task_new() -> *mut c::task_struct {
    let p = unsafe { calloc(1, size_of::<c::task_struct>()) }.cast::<c::task_struct>();
    assert!(!p.is_null(), "error allocating a task_struct");

    unsafe {
        (*p).state = TASK_RUNNING as c_int;
        (*p).pid = rustix::thread::gettid().as_raw_nonzero().get();
        (*p).usage.counter = 1;
    }
    p
}

/// This thread's task_struct, made the first time it's asked for, if
/// kthread_create() didn't make it.
#[unsafe(no_mangle)]
pub extern "C" fn get_current() -> *mut c::task_struct {
    CURRENT.with(|cur| {
        if cur.get().is_null() {
            cur.set(task_new());
        }
        cur.get()
    })
}

/// This thread's task: current.
#[inline(always)]
pub fn current() -> *mut c::task_struct {
    get_current()
}

/// Nothing preempts us here.
#[inline(always)]
pub fn need_resched() -> bool {
    false
}

/// # Safety
/// @task is a task, live.
pub unsafe fn task_pid(task: *mut c::task_struct) -> c_int {
    unsafe { (*task).pid }
}

/// Nothing to tell a scheduler here.
pub fn set_current_wake_cpu(_cpu: c_int) {}

/// A kthread's: its own task_struct, before anything asks for one.
pub(crate) fn set_current(p: *mut c::task_struct) {
    CURRENT.with(|cur| cur.set(p));
}

/*
 * RCU-freed, as the kernel's delayed_put_task_struct(): a waker may hold a
 * task_struct with no ref under rcu_read_lock() - rcuwait does.
 */
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __put_task_struct(t: *mut c::task_struct) {
    unsafe {
        super::kthread::task_free(t);
        c::rcu_free_ptr(t.cast());
    }
}

/* returns true if process was woken up, false if it was already running */
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wake_up_process(p: *mut c::task_struct) -> c_int {
    /*
     * A full barrier before the task state, as try_to_wake_up(): else on
     * arm64 the waker's condition store can land after TASK_RUNNING, and
     * the task overwrites that with its sleep state and reads the
     * condition stale - a lost wakeup (six's lock handoff hung on it).
     */
    fence(SeqCst);

    let s = state(p);
    let ret = s.swap(TASK_RUNNING, Relaxed) != TASK_RUNNING;
    let _ = futex::wake(s, futex::Flags::PRIVATE, i32::MAX as u32);
    ret as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn schedule() {
    let s = state(get_current());

    unsafe { c::rcu_quiescent_state() };

    loop {
        let v = s.load(Acquire);
        if v == TASK_RUNNING {
            break;
        }
        let _ = futex::wait(s, futex::Flags::PRIVATE, v, None);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn schedule_timeout(timeout: c_long) -> c_long {
    /*
     * These two special cases are useful to be comfortable in the caller.
     * Nothing more. We could take MAX_SCHEDULE_TIMEOUT from one of the
     * negative value but I' d like to return a valid offset (>=0) to allow
     * the caller to do everything it want with the retval.
     */
    if timeout == c_long::MAX {
        schedule();
        return timeout;
    }

    /*
     * Another bit of PARANOID. Note that the retval will be 0 since no
     * piece of kernel is supposed to do a check for a negative retval of
     * schedule_timeout() (since it should never happens anyway). You just
     * have the printk() that will tell you if something is gone wrong and
     * where.
     */
    if timeout < 0 {
        eprintln!("schedule_timeout: wrong timeout value {timeout:x}");
        state(get_current()).store(TASK_RUNNING, Relaxed);
        return 0;
    }

    let expire = jiffies().wrapping_add(timeout as c_ulong);
    let ns = (timeout as u64).saturating_mul(NSEC_PER_SEC / HZ as u64);
    let Some(deadline) = Instant::now().checked_add(Duration::from_nanos(ns)) else {
        schedule();
        return timeout;
    };

    let s = state(get_current());

    unsafe { c::rcu_quiescent_state() };

    /* schedule(), till @deadline: run out, it's woken as the kernel's timer would wake it */
    loop {
        let v = s.load(Acquire);
        if v == TASK_RUNNING {
            break;
        }

        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            s.store(TASK_RUNNING, Relaxed);
            break;
        }

        let to = futex::Timespec { tv_sec: left.as_secs() as _, tv_nsec: left.subsec_nanos() as _ };
        let _ = futex::wait(s, futex::Flags::PRIVATE, v, Some(&to));
    }

    (expire.wrapping_sub(jiffies()) as c_long).max(0)
}

/*
 * A thread - a kthread, fuser's workers, a test harness's - registers with
 * RCU and percpu here.
 */
#[unsafe(no_mangle)]
pub extern "C" fn linux_thread_init() {
    let p = get_current();

    unsafe {
        /* only knowable from in here */
        (*p).pid = rustix::thread::gettid().as_raw_nonzero().get();
        c::rcu_register_thread();
        super::percpu::thread_init();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn linux_thread_exit() {
    unsafe { c::rcu_unregister_thread() };
}
