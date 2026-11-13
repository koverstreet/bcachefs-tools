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
use core::sync::atomic::{AtomicU32, Ordering::{Acquire, Relaxed, Release}};

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

/// What a PercpuRef's release does before wait() returns: its owner noticing
/// the last ref is gone.
pub trait OnRelease {
    /// # Safety
    /// @r is the PercpuRef released, and still the owner's.
    unsafe fn released(r: *mut PercpuRef);
}

impl OnRelease for () {
    unsafe fn released(_: *mut PercpuRef) {}
}

unsafe extern "C" fn release<F: OnRelease>(r: *mut c::percpu_ref) {
    // r is a PercpuRef's, at its start
    let r = r.cast::<PercpuRef>();
    unsafe { F::released(r) };
    let released = unsafe { &raw const (*r).released };
    unsafe { (*released).store(1, Release) };
    futex_wake(released.cast(), usize::MAX);
}

impl PercpuRef {
    /// percpu_ref_init(): one ref, the owner's, which stop() puts.
    pub fn new() -> impl ::pin_init::PinInit<Self, BchError> {
        Self::init::<()>(0)
    }

    /// As one stop()ped and waited for, until start(): F's released() runs
    /// each time it's stopped and the last ref is put.
    pub fn new_dead<F: OnRelease>() -> impl ::pin_init::PinInit<Self, BchError> {
        Self::init::<F>(c::PERCPU_REF_INIT_DEAD as u32)
    }

    fn init<F: OnRelease>(flags: u32) -> impl ::pin_init::PinInit<Self, BchError> {
        let dead = flags & c::PERCPU_REF_INIT_DEAD as u32 != 0;
        unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut Self| {
                (&raw mut (*slot).released).write(AtomicU32::new(dead as u32));
                (&raw mut (*slot)._pin).write(PhantomData);
                ret_to_result_void(c::percpu_ref_init(UnsafeCell::raw_get(&raw const (*slot).r),
                                                      Some(release::<F>), flags,
                                                      crate::util::alloc::current_gfp_context_raw(c::GFP_KERNEL)))
            })
        }
    }

    /// Live again, after stop() and wait(): percpu_ref_reinit().
    pub fn start(&self) {
        self.released.store(0, Relaxed);
        unsafe { c::percpu_ref_reinit(self.r.get()) }
    }

    /// Whether no ref is held: stopped, and the last one put.
    pub fn is_zero(&self) -> bool {
        unsafe { c::percpu_ref_is_zero(self.r.get()) }
    }
}

// Zeroed, it was never initialized: percpu_ref_exit() of one frees nothing.
unsafe impl ::pin_init::Zeroable for PercpuRef {}

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
