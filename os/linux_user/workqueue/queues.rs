// SPDX-License-Identifier: GPL-2.0

//! The shim's workqueues: workqueue.rs's, and queue_work() for closure.c.
//!
//! One thread per queue meant a work item that blocks stopped every other item
//! on that queue. For an executor whose tasks are work items (see
//! util/task) that is worse than slow: two tasks racing on one queue, where
//! the first blocks, is a deadlock - the second is what would have woken the
//! first.
//!
//! So: up to @max_active workers per queue, created when work arrives that no
//! free worker is left to take. Cruder than the kernel's concurrency
//! management, which spawns when a worker actually goes to sleep - there is no
//! scheduler hook here for that - but the same shape, and enough that a
//! blocking item cannot own the queue.
//!
//! The guarantee that has to be restored by hand once there is more than one
//! worker is non-reentrancy: the kernel never runs a single work item on two
//! workers at once, and callers rely on it. btree_interior_update_work() is one
//! - it peeks the head of a list and leaves the entry there until it is done
//! with it, so a second concurrent execution picks up the same entry and
//! processes it twice. One worker per queue supplied that exclusion for free;
//! find_runnable_work() is what supplies it now.
//!
//! Delayed work waits in WQ_LOCK's own list, for the thread that queues it
//! when it's due: under the same lock as the queues, so a cancel finds it in
//! one or the other - never between the two, as with a timer that's firing.

use crate::c;
use crate::os::kthread::{kthread_create_name, kthread_should_stop, kthread_stop};
use crate::os::kernel::{jiffies, jiffies_to_msecs, time_after_eq};
use core::ffi::{c_char, c_int, c_uint, c_ulong, c_void, CStr};
use core::ptr::null_mut;
use core::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::collections::VecDeque;
use std::ffi::CString;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

/* What the kernel uses when a caller passes 0. */
const WQ_DFL_ACTIVE: usize = 256;

struct Worker {
    wq:           *mut Workqueue,
    task:         *mut c::task_struct,
    current_work: *mut c::work_struct,
    /* @current_work, queued again while it runs: where, to wake when it's done */
    requeued_on:  *mut Workqueue,
}

/// C's struct workqueue_struct, which it only points to. Its workers and
/// pending work are WQ_LOCK's.
pub struct Workqueue {
    pending_work: VecDeque<*mut c::work_struct>,
    workers:      Vec<*mut Worker>,
    max_active:   usize,
    name:         [u8; 24],
}

impl Workqueue {
    const fn new(name: &[u8], max_active: c_int) -> Self {
        let mut buf = [0; 24];
        let mut i = 0;
        while i < name.len() && i < buf.len() - 1 {
            buf[i] = name[i];
            i += 1;
        }

        Workqueue {
            pending_work: VecDeque::new(),
            workers:      Vec::new(),
            max_active:   if max_active > 0 { max_active as usize } else { WQ_DFL_ACTIVE },
            name:         buf,
        }
    }

    fn name(&self) -> std::borrow::Cow<'_, str> {
        CStr::from_bytes_until_nul(&self.name).expect("NUL terminated").to_string_lossy()
    }
}

/// A delayed work, till @at: queued on @wq then.
struct Delayed {
    at:    c_ulong,
    dwork: *mut c::delayed_work,
    wq:    *mut Workqueue,
}

struct State {
    /// Every queue but the system queues, which are always there
    queues:        Vec<*mut Workqueue>,
    delayed:       Vec<Delayed>,
    delayed_thread: bool,
}

// SAFETY: what it points to is WQ_LOCK's
unsafe impl Send for State {}

static WQ_LOCK: Mutex<State> = Mutex::new(State {
    queues:         Vec::new(),
    delayed:        Vec::new(),
    delayed_thread: false,
});
static WORK_FINISHED: Condvar = Condvar::new();
static DELAYED_ADDED: Condvar = Condvar::new();

/// INIT_WORK(): not pending, and to run @func.
///
/// # Safety
/// @work is a work_struct's place, not queued.
pub(super) unsafe fn init_work(work: *mut c::work_struct, func: super::WorkFn) {
    unsafe {
        (*work).data.counter = 0;
        let entry = &raw mut (*work).entry;
        (*entry).next = entry;
        (*entry).prev = entry;
        (*work).func = Some(func);
    }
}

fn lock() -> MutexGuard<'static, State> {
    WQ_LOCK.lock().expect("workqueue lock poisoned")
}

/* The system queues: WQ_LOCK's, as every queue's insides are */
pub(super) static mut EVENTS:         Workqueue = Workqueue::new(b"events", 0);
pub(super) static mut EVENTS_HIGHPRI: Workqueue = Workqueue::new(b"events_highpri", 0);
pub(super) static mut EVENTS_LONG:    Workqueue = Workqueue::new(b"events_long", 0);
pub(super) static mut EVENTS_UNBOUND: Workqueue = Workqueue::new(b"events_unbound", 512);

fn queues(st: &State) -> impl Iterator<Item = *mut Workqueue> + '_ {
    [&raw mut EVENTS, &raw mut EVENTS_HIGHPRI, &raw mut EVENTS_LONG, &raw mut EVENTS_UNBOUND]
        .into_iter().chain(st.queues.iter().copied())
}

fn nr_idle_workers(wq: *mut Workqueue) -> usize {
    unsafe { (*wq).workers.iter().filter(|&&w| (*w).current_work.is_null()).count() }
}

fn wq_busy(wq: *mut Workqueue) -> bool {
    nr_idle_workers(wq) < unsafe { (*wq).workers.len() }
}

/* Queued work that no free worker is left to take. */
fn work_starved(wq: *mut Workqueue) -> bool {
    unsafe { (*wq).pending_work.len() > nr_idle_workers(wq) }
}

fn work_data<'a>(work: *mut c::work_struct) -> &'a AtomicUsize {
    unsafe { AtomicUsize::from_ptr((&raw mut (*work).data.counter).cast()) }
}

const WORK_PENDING: usize = 1 << 0;

fn work_pending(work: *mut c::work_struct) -> bool {
    work_data(work).load(SeqCst) & WORK_PENDING != 0
}

fn clear_work_pending(work: *mut c::work_struct) {
    work_data(work).fetch_and(!WORK_PENDING, SeqCst);
}

fn set_work_pending(work: *mut c::work_struct) -> bool {
    work_data(work).fetch_or(WORK_PENDING, SeqCst) & WORK_PENDING == 0
}

/*
 * The rule: a given work item is never running more than once at a time.
 *
 * The pending list cannot answer this on its own - queue_work() clears the
 * pending bit before the work function is called, so an item that re-queues
 * itself (or is re-queued by someone else) while running is both pending and
 * running at once. Which worker holds it is the only reliable answer.
 */
fn work_runner(st: &State, work: *mut c::work_struct) -> Option<*mut Worker> {
    queues(st)
        .flat_map(|wq| unsafe { (*wq).workers.iter().copied() })
        .find(|&w| unsafe { (*w).current_work } == work)
}

fn work_running(st: &State, work: *mut c::work_struct) -> bool {
    work_runner(st, work).is_some()
}

/*
 * The oldest queued item nobody is already running - see the non-reentrancy
 * note above. Skipping past a busy item rather than waiting behind it is what
 * the kernel does too, and it is the whole point of having more than one
 * worker: an item that blocks must not hold up unrelated ones.
 */
fn find_runnable_work(st: &State, wq: *mut Workqueue) -> Option<usize> {
    unsafe { (*wq).pending_work.iter().position(|&work| !work_running(st, work)) }
}

/*
 * Which idle worker takes which item is settled by who gets WQ_LOCK, and the
 * losers go straight back to sleep. A worker that is running something is
 * deliberately left alone: it re-checks the pending list under WQ_LOCK before
 * it sleeps, so waking it would only cost a futex call.
 */
fn wake_idle_workers(wq: *mut Workqueue) {
    for &w in unsafe { &(*wq).workers } {
        unsafe {
            if (*w).current_work.is_null() {
                c::wake_up_process((*w).task);
            }
        }
    }
}

unsafe extern "C" fn worker_thread(arg: *mut c_void) -> c_int {
    let worker = arg.cast::<Worker>();
    let wq = unsafe { (*worker).wq };

    let mut st = lock();
    loop {
        crate::os::sched::set_current_state(c::TASK_INTERRUPTIBLE);

        if kthread_should_stop() {
            assert!(unsafe { (*wq).pending_work.is_empty() }, "workqueue stopped with work pending");
            break;
        }

        let Some(i) = find_runnable_work(&st, wq) else {
            drop(st);
            unsafe { c::schedule() };
            st = lock();
            continue;
        };
        crate::os::sched::set_current_state_running();

        let work = unsafe { (*wq).pending_work.remove(i) }.expect("found");
        assert!(work_pending(work));
        clear_work_pending(work);
        unsafe { (*worker).current_work = work };

        drop(st);
        unsafe { (*work).func.expect("a work item's function")(work) };
        st = lock();

        /*
         * Before the broadcast, not after the next loop iteration
         * overwrites it: a flush_work() waiter woken here would
         * otherwise still see its own work as running, wait again, and
         * never be signalled - the clear did not broadcast.
         */
        unsafe { (*worker).current_work = null_mut() };
        WORK_FINISHED.notify_all();

        /*
         * Anything still queued may have been passed over by a worker
         * that then slept - including because it was this very item,
         * skipped for non-reentrancy. That includes another queue's
         * workers, if the item was queued there while it ran here: an
         * async task, requeued to another queue from inside its poll.
         */
        if unsafe { !(*wq).pending_work.is_empty() } {
            wake_idle_workers(wq);
        }
        let requeued_on = unsafe { core::mem::replace(&mut (*worker).requeued_on, null_mut()) };
        if !requeued_on.is_null() {
            wake_idle_workers(requeued_on);
        }
    }
    drop(st);
    0
}

fn __queue_work(st: &mut State, wq: *mut Workqueue, work: *mut c::work_struct) {
    assert!(work_pending(work));

    unsafe { (*wq).pending_work.push_back(work) };

    /*
     * Running already, on a worker of this queue or another: this queue's
     * workers pass it over until that run ends, so the worker running it
     * wakes them then - after its run it can't look at the item, which may
     * be freed.
     */
    if let Some(w) = work_runner(st, work) {
        unsafe { (*w).requeued_on = wq };
    }

    /*
     * Nobody free to take it, and room to grow: another worker. The first
     * item on a fresh queue takes this path, so a queue nothing blocks on
     * settles at exactly one thread.
     *
     * Counting idle workers by walking them, rather than keeping a counter
     * maintained on the sleep path, is the difference between this working
     * and not: a worker that has been woken but has not yet reacquired
     * WQ_LOCK still looks idle by a counter, and the item it is about to
     * take is the one that would have justified the new worker.
     */
    unsafe {
        if work_starved(wq) && (*wq).workers.len() < (*wq).max_active {
            let w = Box::into_raw(Box::new(Worker {
                wq,
                task:         null_mut(),
                current_work: null_mut(),
                requeued_on:  null_mut(),
            }));

            let name = CString::new(format!("{}/{}", (*wq).name(), (*wq).workers.len()))
                .expect("no NULs in a workqueue's name");
            let task = kthread_create_name(Some(worker_thread), w.cast(), name.as_ptr());
            let ret = task as isize;
            if (-4095..0).contains(&ret) {
                panic!("error creating workqueue thread: {}",
                       std::io::Error::from_raw_os_error(-ret as i32));
            }

            (*w).task = task;
            (*wq).workers.push(w);
            c::wake_up_process(task);
        }
    }

    wake_idle_workers(wq);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn queue_work(wq: *mut c::workqueue_struct, work: *mut c::work_struct) -> bool {
    let mut st = lock();
    let ret = set_work_pending(work);
    if ret {
        __queue_work(&mut st, wq.cast(), work);
    }
    ret
}

fn delayed_work_thread() {
    let mut st = lock();
    loop {
        let now = jiffies();

        let mut i = 0;
        while i < st.delayed.len() {
            if time_after_eq(now, st.delayed[i].at) {
                let d = st.delayed.swap_remove(i);
                __queue_work(&mut st, d.wq, unsafe { &raw mut (*d.dwork).work });
            } else {
                i += 1;
            }
        }

        st = match st.delayed.iter().map(|d| d.at.wrapping_sub(now)).min() {
            None    => DELAYED_ADDED.wait(st).expect("workqueue lock poisoned"),
            Some(j) => DELAYED_ADDED.wait_timeout(st, Duration::from_millis(jiffies_to_msecs(j).into()))
                .expect("workqueue lock poisoned").0,
        };
    }
}

fn __queue_delayed_work(st: &mut State, wq: *mut Workqueue, dwork: *mut c::delayed_work, delay: c_ulong) {
    if delay == 0 {
        __queue_work(st, wq, unsafe { &raw mut (*dwork).work });
        return;
    }

    st.delayed.push(Delayed { at: jiffies().wrapping_add(delay), dwork, wq });
    if !st.delayed_thread {
        std::thread::Builder::new()
            .name("delayed_work".into())
            .spawn(delayed_work_thread)
            .expect("error creating the delayed work thread");
        st.delayed_thread = true;
    }
    DELAYED_ADDED.notify_one();
}

pub unsafe fn queue_delayed_work(wq: *mut c::workqueue_struct, dwork: *mut c::delayed_work,
                                 delay: c_ulong) -> bool {
    let mut st = lock();
    let ret = set_work_pending(unsafe { &raw mut (*dwork).work });
    if ret {
        __queue_delayed_work(&mut st, wq.cast(), dwork, delay);
    }
    ret
}

/// @work's pending bit, ours: taken off whatever queue or delayed list it's
/// on - whether it was pending.
fn grab_pending(st: &mut State, work: *mut c::work_struct, is_dwork: bool) -> bool {
    if set_work_pending(work) {
        return false;
    }

    if is_dwork {
        const { assert!(core::mem::offset_of!(c::delayed_work, work) == 0) };
        let dwork = work.cast::<c::delayed_work>();

        if let Some(i) = st.delayed.iter().position(|d| d.dwork == dwork) {
            st.delayed.swap_remove(i);
            return true;
        }
    }

    for wq in queues(st) {
        let pending = unsafe { &mut (*wq).pending_work };
        if let Some(i) = pending.iter().position(|&w| w == work) {
            pending.remove(i);
            return true;
        }
    }

    unreachable!("work pending, but neither queued nor delayed");
}

pub unsafe fn flush_work(work: *mut c::work_struct) -> bool {
    let mut ret = false;

    let mut st = lock();
    while work_pending(work) || work_running(&st, work) {
        st = WORK_FINISHED.wait(st).expect("workqueue lock poisoned");
        ret = true;
    }
    ret
}

fn __flush_work(mut st: MutexGuard<'static, State>, work: *mut c::work_struct) -> MutexGuard<'static, State> {
    while work_running(&st, work) {
        st = WORK_FINISHED.wait(st).expect("workqueue lock poisoned");
    }
    st
}

pub unsafe fn cancel_work_sync(work: *mut c::work_struct) -> bool {
    let mut st = lock();
    let ret = grab_pending(&mut st, work, false);

    let _st = __flush_work(st, work);
    clear_work_pending(work);
    ret
}

pub unsafe fn mod_delayed_work(wq: *mut c::workqueue_struct, dwork: *mut c::delayed_work,
                               delay: c_ulong) -> bool {
    let mut st = lock();
    let ret = grab_pending(&mut st, unsafe { &raw mut (*dwork).work }, true);

    __queue_delayed_work(&mut st, wq.cast(), dwork, delay);
    ret
}

pub unsafe fn cancel_delayed_work(dwork: *mut c::delayed_work) -> bool {
    let work = unsafe { &raw mut (*dwork).work };

    let mut st = lock();
    let ret = grab_pending(&mut st, work, true);

    clear_work_pending(work);
    ret
}

pub unsafe fn cancel_delayed_work_sync(dwork: *mut c::delayed_work) -> bool {
    let work = unsafe { &raw mut (*dwork).work };

    let mut st = lock();
    let ret = grab_pending(&mut st, work, true);

    let _st = __flush_work(st, work);
    clear_work_pending(work);
    ret
}

pub unsafe fn drain_workqueue(wq: *mut c::workqueue_struct) {
    let wq = wq.cast::<Workqueue>();

    let mut st = lock();
    while unsafe { !(*wq).pending_work.is_empty() } || wq_busy(wq) {
        st = WORK_FINISHED.wait(st).expect("workqueue lock poisoned");
    }
}

pub unsafe fn destroy_workqueue(wq: *mut c::workqueue_struct) {
    let wq = wq.cast::<Workqueue>();

    /*
     * kthread_stop() waits for the worker, and the worker needs WQ_LOCK to
     * get out of its loop - so stop them all without it. Freeing them is
     * the part that needs care: work_running() walks every queue's workers
     * from other threads, so nothing may be freed until this queue is off
     * the list and unreachable.
     */
    let tasks: Vec<_> = {
        let _st = lock();
        unsafe { (*wq).workers.iter().map(|&w| (*w).task).collect() }
    };
    for task in tasks {
        unsafe { kthread_stop(task) };
    }

    lock().queues.retain(|&q| q != wq);

    let wq = unsafe { Box::from_raw(wq) };
    for &w in &wq.workers {
        drop(unsafe { Box::from_raw(w) });
    }
}

/// alloc_workqueue(), the name formatted. @flags are the kernel's, and
/// ignored: one kind of worker here.
pub unsafe fn alloc_workqueue_name(name: *const c_char, _flags: c_uint,
                                   max_active: c_int) -> *mut c::workqueue_struct {
    let name = unsafe { CStr::from_ptr(name) };

    /*
     * Whatever the caller asked for. Capping it lower looks harmless -
     * threads are not free and 512 is a lot - but max_active is the bound
     * on how many of this queue's items may be blocked at once, so a queue
     * whose callers can block more items than we allow workers deadlocks
     * exactly like the one worker per queue this replaced. It costs
     * nothing to honour: workers are created only when queued work has no
     * free worker to take it, so a queue nothing blocks on still settles
     * at one thread no matter what this says.
     */
    let wq = Box::into_raw(Box::new(Workqueue::new(name.to_bytes(), max_active)));

    lock().queues.push(wq);
    wq.cast()
}
