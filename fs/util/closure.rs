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
use crate::util::ffi::Opaque;
use crate::util::refcount::{RefGuard, Refcount, TryRefcount};
use core::cell::UnsafeCell;
use core::marker::PhantomPinned;
use core::pin::Pin;
use core::ptr::NonNull;
use pin_init::{pin_data, pinned_drop, PinInit};

/// A ref on a closure. Its tryget() is closure_get_not_zero(): it fails once
/// the closure has none left - it's finished, or finishing.
///
/// On an Opaque: whoever holds a ref may put it from any thread, writing the
/// closure under every other holder's shared reference.
pub type ClosureRef = RefGuard<Opaque<c::closure>>;

// SAFETY: a closure's refcount is atomic, changed through a shared pointer;
// closure_get() and a closure_get_not_zero() that succeeds take a ref
unsafe impl Refcount for Opaque<c::closure> {
    fn inc(&self) {
        // SAFETY: as above
        unsafe { c::closure_get(self.as_ptr()) }
    }

    unsafe fn dec(&self) {
        // SAFETY: the caller's ref
        unsafe { c::closure_put(self.as_ptr()) }
    }
}

// SAFETY: as Refcount's
unsafe impl TryRefcount for Opaque<c::closure> {
    fn try_inc(&self) -> bool {
        // SAFETY: as above
        unsafe { c::closure_get_not_zero(self.as_ptr()) }
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

    /// As a closure C handed over: what's written for one takes this too.
    pub fn as_opaque(&self) -> &Opaque<c::closure> {
        // SAFETY: Opaque is a transparent UnsafeCell, as @cl is; initialized
        unsafe { &*self.cl.get().cast::<Opaque<c::closure>>() }
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

/// Whether anything's waiting on @list - a hint, racing its adds and wakes.
pub fn closure_has_waiters(list: &c::closure_waitlist) -> bool {
    // SAFETY: a lockless list's head, which its adds and wakes swap: read
    // once, as READ_ONCE()
    !unsafe { core::ptr::read_volatile(&raw const list.list.first) }.is_null()
}

/// Each waiter's ref put.
pub fn closure_wake_up(list: &c::closure_waitlist) {
    // SAFETY: as closure_wait()'s
    unsafe { c::closure_wake_up(core::ptr::from_ref(list).cast_mut()) }
}

/// Sleep until @cond, woken by @list's closure_wake_up()s: C's
/// closure_wait_event(). @cond is checked on the list, so a wake between the
/// check and the sleep isn't lost. For a list C wakes - a wait Rust has end
/// to end is a futex's, util/futex.rs.
pub fn closure_wait_event(list: &c::closure_waitlist, mut cond: impl FnMut() -> bool) {
    if cond() {
        return;
    }

    pin_init::stack_pin_init!(let cl = StackClosure::new());
    let clp = NonNull::new(cl.as_ptr()).expect("a closure on the stack");
    loop {
        // SAFETY: ours, waiting on nothing else - each wake_up() puts the ref
        // this takes before the next
        unsafe { closure_wait(list, clp) };
        if cond() {
            break;
        }
        cl.sync();
    }
    // off the waitlist, before it goes
    closure_wake_up(list);
}
