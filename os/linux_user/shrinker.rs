// SPDX-License-Identifier: GPL-2.0

//! A shrinker: memory reclaim asking what it frees - a Shrink - how much it
//! could, and to free some, from any thread while it's registered.
//!
//! The Shrink holds its own Shrinker, and is placed: so the owner the
//! callbacks are given stays where it is, as long as what it's in. Its
//! PlacedDrop unregisters it, while what count() and scan() use is still
//! there; the Shrinker's own drop checks that it did.
//!
//! The shim's reclaim is here too: the registered shrinkers, run when an
//! allocation fails - run_shrinkers(), slab.h's C allocators' - and once a second by the
//! shrinkers thread, when free memory's short.

use core::ffi::{c_int, c_ulong, c_void, CStr};
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicPtr, Ordering::{AcqRel, Acquire, Relaxed}};
use std::sync::{Mutex, MutexGuard};

use super::kthread::{kthread_create_name, kthread_should_stop};
use super::sched::{linux_thread_init, schedule_timeout, wake_up_process};
use crate::c;
use crate::os::kernel::{HZ, PAGE_SIZE};
use crate::util::alloc::AllocError;
use crate::util::place::{Contained, PlacedDrop};

pub(crate) trait Shrink: Contained + PlacedDrop + Sync {
    fn shrinker(&self) -> &Shrinker<Self>;

    /// How many objects scan() could free now.
    fn count(&self) -> usize;

    /// Free up to @nr: how many were freed - or None, for reclaim to stop
    /// asking for now: SHRINK_STOP.
    fn scan(&self, nr: usize) -> Option<usize>;
}

/// A registered shrinker: its owner, and count::<T>() and scan::<T>() of it.
struct Registered {
    owner: *const (),
    count: unsafe fn(*const ()) -> usize,
    scan:  unsafe fn(*const (), usize) -> Option<usize>,
}

/// The registered shrinkers
struct Shrinkers(Vec<*mut Registered>);

// SAFETY: what it points to is SHRINKERS's
unsafe impl Send for Shrinkers {}

static SHRINKERS: Mutex<Shrinkers> = Mutex::new(Shrinkers(Vec::new()));

fn lock() -> MutexGuard<'static, Shrinkers> {
    SHRINKERS.lock().expect("shrinker lock poisoned")
}

/// Null until register(), and after unregister().
#[repr(transparent)]
pub(crate) struct Shrinker<T> {
    raw: AtomicPtr<Registered>,
    _t:  PhantomData<fn(&T)>,
}

// SAFETY: null, unregistered
unsafe impl<T> pin_init::Zeroable for Shrinker<T> {}

impl<T: Shrink> Shrinker<T> {
    /// @owner's, registered: @seeks is what an object costs to recreate,
    /// DEFAULT_SEEKS 2; @batch how many a scan() is asked to free, 0 for the
    /// default. Neither is kept, nor its name: nothing here reads them.
    pub(crate) fn register(owner: &T, name: &CStr, _seeks: c_int, _batch: usize) -> Result<(), AllocError> {
        let s = Box::into_raw(Box::new(Registered {
            owner: core::ptr::from_ref(owner).cast(),
            count: count::<T>,
            scan:  scan::<T>,
        }));

        // published only registered: unregister() frees only what is
        if owner.shrinker().raw.compare_exchange(null_mut(), s, AcqRel, Relaxed).is_err() {
            drop(unsafe { Box::from_raw(s) });
            panic!("shrinker {name:?} registered twice");
        }
        lock().0.push(s);
        Ok(())
    }

    /// What an object costs to recreate, changed: nothing here reads it.
    pub(crate) fn set_seeks(&self, _seeks: c_int) {}

    /// Unregistered, any count() or scan() still running waited out: they
    /// run under the lock this takes.
    ///
    /// # Safety
    /// Nothing else uses it meanwhile: the owner's placed_drop() calls this.
    pub(crate) unsafe fn unregister(&self) {
        let s = self.raw.swap(null_mut(), Acquire);
        if !s.is_null() {
            lock().0.retain(|&i| i != s);
            drop(unsafe { Box::from_raw(s) });
        }
    }
}

impl<T> Drop for Shrinker<T> {
    fn drop(&mut self) {
        assert!(self.raw.get_mut().is_null(), "a shrinker dropped registered: its owner's placed_drop() wasn't called");
    }
}

unsafe fn count<T: Shrink>(owner: *const ()) -> usize {
    unsafe { &*owner.cast::<T>() }.count()
}

unsafe fn scan<T: Shrink>(owner: *const (), nr: usize) -> Option<usize> {
    unsafe { &*owner.cast::<T>() }.scan(nr)
}

// ── Reclaim ────────────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub static mut _totalram_pages: c_ulong = 0;

fn run_shrinkers_allocation_failed() {
    for &s in &lock().0 {
        unsafe {
            let have = ((*s).count)((*s).owner);
            ((*s).scan)((*s).owner, have / 8);
        }
    }
}

fn si_meminfo() -> c::sysinfo {
    let mut info = MaybeUninit::uninit();
    unsafe {
        c::si_meminfo(info.as_mut_ptr());
        info.assume_init()
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn run_shrinkers(gfp_mask: c::gfp_t, allocation_failed: bool) {
    if gfp_mask & c::GFP_KERNEL == 0 {
        return;
    }

    /* Fast out if there are no shrinkers to run. */
    if lock().0.is_empty() {
        return;
    }

    if allocation_failed {
        run_shrinkers_allocation_failed();
        return;
    }

    let info = si_meminfo();

    /* Aim for 6% of physical RAM free without anything in swap */
    let want_shrink = (info.totalram >> 4) as i64 - info.freeram as i64
        + info.totalswap as i64 - info.freeswap as i64;
    if want_shrink <= 0 {
        return;
    }

    for &s in &lock().0 {
        unsafe { ((*s).scan)((*s).owner, want_shrink as usize / PAGE_SIZE) };
    }
}

unsafe extern "C" fn shrinker_thread(_arg: *mut c_void) -> c_int {
    while !kthread_should_stop() {
        super::sched::set_current_state(c::TASK_INTERRUPTIBLE);
        schedule_timeout(HZ as _);

        if kthread_should_stop() {
            break;
        }
        run_shrinkers(c::GFP_KERNEL, false);
    }
    0
}

pub fn linux_shrinkers_init() {
    unsafe { c::rcu_init() };
    linux_thread_init();

    super::blkdev::init();

    let info = si_meminfo();
    unsafe { _totalram_pages = info.totalram / PAGE_SIZE as c_ulong };

    let task = unsafe { kthread_create_name(Some(shrinker_thread), null_mut(), c"shrinkers".as_ptr()) };
    assert!(!(-4095..0).contains(&(task as isize)), "error creating the shrinkers thread");
    unsafe { wake_up_process(task) };
}
