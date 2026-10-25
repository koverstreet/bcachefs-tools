// SPDX-License-Identifier: GPL-2.0

//! C's closures (vendor/closure.h), where Rust meets C that's built on them -
//! to go as that C does:
//!
//! - refs on a closure: closure_return_sync() waits for them, so a walker can
//!   use what it found after dropping the lock it found it under;
//! - a closure on the stack, to hand to C that waits by putting it on a
//!   waitlist - the allocator - and to sync on;
//! - a waitlist, for what C waits on with its closure: a stripe buffer's
//!   memory, whose waiters are the write path's.

use crate::c;
use core::cell::UnsafeCell;
use core::marker::PhantomPinned;
use core::pin::Pin;
use core::ptr::NonNull;
use pin_init::{pin_data, pinned_drop, PinInit};

/// A ref on a closure, put on drop: closure_put().
#[must_use = "the ref is put as soon as it's dropped"]
pub struct ClosureRef<'a>(&'a c::closure);

/// closure_get_not_zero(): a ref, unless the closure has none left - it's
/// finished, or finishing.
pub fn closure_get_not_zero(cl: &c::closure) -> Option<ClosureRef<'_>> {
    // SAFETY: a closure's refcount is atomic, changed through a shared pointer
    unsafe { c::closure_get_not_zero(core::ptr::from_ref(cl).cast_mut()) }
        .then(|| ClosureRef(cl))
}

impl Drop for ClosureRef<'_> {
    fn drop(&mut self) {
        // SAFETY: the ref this holds
        unsafe { c::closure_put(core::ptr::from_ref(self.0).cast_mut()) }
    }
}

/// C's CLASS(closure_stack). Dropped, it syncs, so nothing's left holding it.
#[pin_data(PinnedDrop)]
pub struct StackClosure {
    cl:   UnsafeCell<c::closure>,
    #[pin]
    _pin: PhantomPinned,
}

impl StackClosure {
    pub fn new() -> impl PinInit<Self> {
        // SAFETY: closure_init_stack() initializes all of it, in place
        unsafe {
            ::pin_init::pin_init_from_closure(|slot: *mut Self| {
                c::closure_init_stack(UnsafeCell::raw_get(&raw mut (*slot).cl));
                Ok(())
            })
        }
    }

    pub fn as_ptr(&self) -> *mut c::closure {
        self.cl.get()
    }

    /// Its refs: one of its own, and one for each waitlist it's on.
    pub fn nr_remaining(&self) -> u32 {
        // SAFETY: initialized; the count is atomic
        unsafe { c::closure_nr_remaining(self.as_ptr()) }
    }

    pub fn sync(&self) {
        // SAFETY: initialized; its refcount atomic, the rest the waitlist's
        unsafe { c::closure_sync(self.as_ptr()) }
    }
}

#[pinned_drop]
impl PinnedDrop for StackClosure {
    fn drop(self: Pin<&mut Self>) {
        self.sync()
    }
}

/// A ref on @cl, until closure_wake_up(). False if it's waiting already.
///
/// # Safety
/// @cl is a live closure, waiting on nothing else.
pub unsafe fn closure_wait(list: &c::closure_waitlist, cl: NonNull<c::closure>) -> bool {
    // SAFETY: a waitlist is a lockless list, added to through a shared
    // pointer; @cl the caller's
    unsafe { c::bch2_closure_wait(core::ptr::from_ref(list).cast_mut(), cl.as_ptr()) }
}

/// Each waiter's ref put.
pub fn closure_wake_up(list: &c::closure_waitlist) {
    // SAFETY: as closure_wait()'s
    unsafe { c::closure_wake_up(core::ptr::from_ref(list).cast_mut()) }
}
