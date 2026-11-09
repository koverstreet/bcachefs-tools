// SPDX-License-Identifier: GPL-2.0

//! A shrinker: memory reclaim asking what it frees - a Shrink - how much it
//! could, and to free some, from any thread while it's registered.
//!
//! The Shrink holds its own Shrinker, and is placed: so the owner the
//! callbacks are given stays where it is, as long as what it's in. Its
//! PlacedDrop unregisters it, while what count() and scan() use is still
//! there; the Shrinker's own drop checks that it did.

use core::ffi::{c_int, CStr};
use core::marker::PhantomData;
use core::sync::atomic::{AtomicPtr, Ordering::{AcqRel, Acquire, Relaxed}};

use crate::c;
use crate::util::alloc::AllocError;
use crate::util::place::{Contained, PlacedDrop};

// struct shrinker's: the kernel crate's bindings', in-kernel, which have
// unsigned long as usize
#[cfg(kernel)]
use kernel::ffi::c_ulong;
#[cfg(not(kernel))]
use core::ffi::c_ulong;

pub(crate) trait Shrink: Contained + PlacedDrop + Sync {
    fn shrinker(&self) -> &Shrinker<Self>;

    /// How many objects scan() could free now.
    fn count(&self) -> usize;

    /// Free up to @nr: how many were freed - or None, for reclaim to stop
    /// asking for now: SHRINK_STOP.
    fn scan(&self, nr: usize) -> Option<usize>;
}

/// C's struct shrinker *: null until register(), and after unregister().
#[repr(transparent)]
pub(crate) struct Shrinker<T> {
    raw: AtomicPtr<c::shrinker>,
    _t:  PhantomData<fn(&T)>,
}

// SAFETY: null, unregistered
unsafe impl<T> pin_init::Zeroable for Shrinker<T> {}

impl<T: Shrink> Shrinker<T> {
    /// @owner's, named @name, registered: @seeks is what an object costs to
    /// recreate, DEFAULT_SEEKS 2; @batch how many a scan() is asked to free,
    /// 0 for the default.
    pub(crate) fn register(owner: &T, name: &CStr, seeks: c_int, batch: usize) -> Result<(), AllocError> {
        let s = unsafe { c::shrinker_alloc(0, c"%s".as_ptr().cast(), name.as_ptr()) };
        if s.is_null() {
            return Err(AllocError);
        }
        unsafe {
            (*s).count_objects = Some(count::<T>);
            (*s).scan_objects  = Some(scan::<T>);
            (*s).seeks         = seeks;
            (*s).batch         = batch as _;
            (*s).private_data  = core::ptr::from_ref(owner).cast_mut().cast();
            c::shrinker_register(s);
        }
        // published only registered: unregister() frees only what is
        if owner.shrinker().raw.compare_exchange(core::ptr::null_mut(), s, AcqRel, Relaxed).is_err() {
            unsafe { c::shrinker_free(s) };
            panic!("shrinker {name:?} registered twice");
        }
        Ok(())
    }

    /// What an object costs to recreate, changed: a no-op unregistered.
    pub(crate) fn set_seeks(&self, seeks: c_int) {
        let s = self.raw.load(Acquire);
        if !s.is_null() {
            // registered, so live: unregister()'s caller says nothing's here
            unsafe { (*s).seeks = seeks };
        }
    }

    /// Unregistered, any count() or scan() still running waited out.
    ///
    /// # Safety
    /// Nothing else uses it meanwhile: the owner's placed_drop() calls this.
    pub(crate) unsafe fn unregister(&self) {
        let s = self.raw.swap(core::ptr::null_mut(), Acquire);
        if !s.is_null() {
            unsafe { c::shrinker_free(s) }
        }
    }
}

impl<T> Drop for Shrinker<T> {
    fn drop(&mut self) {
        assert!(self.raw.get_mut().is_null(), "a shrinker dropped registered: its owner's placed_drop() wasn't called");
    }
}

/// SHRINK_STOP: ~0UL.
const SHRINK_STOP: c_ulong = !0;

unsafe extern "C" fn count<T: Shrink>(s: *mut c::shrinker, _sc: *mut c::shrink_control) -> c_ulong {
    let owner = unsafe { &*(*s).private_data.cast::<T>() };
    owner.count() as c_ulong
}

unsafe extern "C" fn scan<T: Shrink>(s: *mut c::shrinker, sc: *mut c::shrink_control) -> c_ulong {
    let owner = unsafe { &*(*s).private_data.cast::<T>() };
    let nr = unsafe { (*sc).nr_to_scan } as usize;
    owner.scan(nr).map_or(SHRINK_STOP, |freed| freed as c_ulong)
}
