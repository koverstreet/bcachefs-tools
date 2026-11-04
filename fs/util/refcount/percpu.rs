// SPDX-License-Identifier: GPL-2.0

//! PercpuRef: a percpu_ref, as a WaitableRef - stop() kills it, wait()
//! returns once its release has run.
//!
//! The release sets released and wakes it as a futex: that store is the last
//! it makes of the PercpuRef, and the wake only hashes the address - so
//! wait() returning, and the owner freeing it, can't race a release still
//! touching it. The count reaching zero comes before the release, in the
//! percpu_ref's own data: waiting on that would race it.
//!
//! C sees what holds one declare it: #[c_anon("struct { struct percpu_ref
//! ref; u32 ref_released; }")], C's percpu_ref where it was - in a struct
//! of its own, so C pads it as Rust does.

use core::cell::UnsafeCell;
use core::marker::{PhantomData, PhantomPinned};
use core::sync::atomic::{AtomicU32, Ordering::{Acquire, Release}};

use super::{Refcount, TryRefcount, WaitableRef};
use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::util::futex::{futex_wait, futex_wake};
use crate::util::sleep::Sleep;

#[repr(C)]
pub struct PercpuRef {
    r:        UnsafeCell<c::percpu_ref>,
    released: AtomicU32,
    // pinned - a percpu_ref's data points back to it - in a PhantomData: a
    // bare PhantomPinned isn't FFI-safe in a repr(C) struct
    _pin:     PhantomData<PhantomPinned>,
}

// SAFETY: a percpu_ref is got and put from any thread
unsafe impl Send for PercpuRef {}
unsafe impl Sync for PercpuRef {}

unsafe extern "C" fn release(r: *mut c::percpu_ref) {
    // r is a PercpuRef's, at its start
    let released = unsafe { &raw const (*r.cast::<PercpuRef>()).released };
    unsafe { (*released).store(1, Release) };
    futex_wake(released.cast(), usize::MAX);
}

impl PercpuRef {
    /// percpu_ref_init(): one ref, the owner's, which stop() puts.
    pub fn new() -> impl ::pin_init::PinInit<Self, BchError> {
        unsafe {
            ::pin_init::pin_init_from_closure(|slot: *mut Self| {
                (&raw mut (*slot).released).write(AtomicU32::new(0));
                (&raw mut (*slot)._pin).write(PhantomData);
                ret_to_result_void(c::percpu_ref_init(UnsafeCell::raw_get(&raw const (*slot).r),
                                                      Some(release), 0,
                                                      crate::util::alloc::current_gfp_context_raw(c::GFP_KERNEL)))
            })
        }
    }
}

impl Drop for PercpuRef {
    fn drop(&mut self) {
        unsafe { c::percpu_ref_exit(self.r.get()) }
    }
}

unsafe impl Refcount for PercpuRef {
    unsafe fn inc(&self) {
        unsafe { c::percpu_ref_get(self.r.get()) }
    }

    unsafe fn dec(&self) {
        unsafe { c::percpu_ref_put(self.r.get()) }
    }
}

unsafe impl TryRefcount for PercpuRef {
    fn try_inc(&self) -> bool {
        unsafe { c::percpu_ref_tryget(self.r.get()) }
    }

    fn try_inc_live(&self) -> bool {
        unsafe { c::percpu_ref_tryget_live(self.r.get()) }
    }
}

impl WaitableRef for PercpuRef {
    fn stop(&self) {
        unsafe { c::percpu_ref_kill(self.r.get()) }
    }

    fn wait(&self) {
        while self.released.load(Acquire) == 0 {
            let Ok(()) = futex_wait(&self.released, 0, Sleep::UNINTERRUPTIBLE);
        }
    }
}
