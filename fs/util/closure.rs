// SPDX-License-Identifier: GPL-2.0

//! C's closures (vendor/closure.h), where Rust meets C that's built on them -
//! to go as that C does:
//!
//! - refs on a closure: closure_return_sync() waits for them, so a walker can
//!   use what it found after dropping the lock it found it under;
//! - a closure on the stack, to hand to C that waits by putting it on a
//!   waitlist - the allocator - and to sync on;
//! - a waitlist, for what C waits on with its closure: a stripe buffer's
//!   memory, whose waiters are the write path's;
//! - a closure as the frame of an op its fns run in turn: Running, the
//!   write path's.

use crate::c;
use crate::util::ffi::Opaque;
use crate::util::refcount::{RefBorrow, RefGuard, Refcount, TryRefcount};
use crate::util::workqueue::Queue;
use core::cell::UnsafeCell;
use core::marker::{PhantomData, PhantomPinned};
use core::ops::{Deref, DerefMut};
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
    unsafe fn inc(&self) {
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

// A zeroed closure is one closure_init() hasn't run on yet, as in C's
// kzalloc()'d structs: what closure::call() starts
unsafe impl ::pin_init::Zeroable for Opaque<c::closure> {}

impl Opaque<c::closure> {
    /// Its refs: the running ref, while it runs, and every other.
    pub fn nr_remaining(&self) -> u32 {
        unsafe { c::closure_nr_remaining(self.as_ptr()) }
    }

    /// Whether it puts a ref on a parent when it's done.
    pub fn has_parent(&self) -> bool {
        !unsafe { (*self.as_ptr()).parent }.is_null()
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
        self.as_opaque().nr_remaining()
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

/// T's closure, CLOSURE bytes into it: has_closure!().
///
/// # Safety
/// Every T has a c::closure at CLOSURE.
pub unsafe trait HasClosure: Sized {
    const CLOSURE: usize;

    /// The T @cl is the closure of.
    ///
    /// # Safety
    /// @cl is a T's closure.
    unsafe fn of_closure(cl: *mut c::closure) -> *mut Self {
        cl.wrapping_byte_sub(Self::CLOSURE).cast()
    }
}

#[doc(hidden)]
pub trait ClosureField {}
impl ClosureField for c::closure {}
impl ClosureField for Opaque<c::closure> {}

#[doc(hidden)]
pub const fn closure_field<C: ClosureField>(_: *const C) {}

/// HasClosure for $t, by its closure field $f.
#[macro_export]
macro_rules! has_closure {
    ($t:ty, $($f:ident).+) => {
        unsafe impl $crate::util::closure::HasClosure for $t {
            const CLOSURE: usize = {
                let t = ::core::mem::MaybeUninit::<$t>::uninit();
                $crate::util::closure::closure_field(unsafe { &raw const (*t.as_ptr()).$($f).+ });
                ::core::mem::offset_of!($t, $($f).+)
            };
        }
    };
}

/// A closure fn: C's CLOSURE_CALLBACK(), closure_type() its argument's.
pub type ClosureFn<T> = extern "C" fn(Running<T>);

/// The ref a closure's running code holds, which keeps its T: closure_call()'s,
/// or the one a closure fn is run with. There's one, and it's given up by
/// continue_at(), end() or into_raw() - never dropped.
///
/// What C passes a closure fn, &cl->work - work is first in the closure - so a
/// ClosureFn<T> is a closure_fn: transparent over the same pointer type, which
/// is what kCFI hashes it as.
#[repr(transparent)]
pub struct Running<T: HasClosure> {
    work: *mut c::work_struct,
    _t:   PhantomData<*mut T>,
}

const _: () = assert!(core::mem::offset_of!(c::closure, __bindgen_anon_1) == 0);

fn closure_fn<T: HasClosure>(f: ClosureFn<T>) -> c::closure_fn {
    Some(unsafe { core::mem::transmute::<ClosureFn<T>, unsafe extern "C" fn(*mut c::work_struct)>(f) })
}

fn wq_ptr(wq: Option<&Queue>) -> *mut c::workqueue_struct {
    wq.map_or(core::ptr::null_mut(), Queue::as_ptr)
}

impl<T: HasClosure> Running<T> {
    /// A running ref handed on, as into_raw() gave it up.
    ///
    /// # Safety
    /// @this's closure's running ref is the caller's, to give to this.
    pub unsafe fn from_raw(this: NonNull<T>) -> Self {
        Running { work: this.as_ptr().wrapping_byte_add(T::CLOSURE).cast(), _t: PhantomData }
    }

    /// Given up to whoever from_raw()s it: still running, its ref not put.
    pub fn into_raw(self) -> NonNull<T> {
        let this = self.this();
        core::mem::forget(self);
        this
    }

    fn this(&self) -> NonNull<T> {
        unsafe { NonNull::new_unchecked(self.work.wrapping_byte_sub(T::CLOSURE).cast()) }
    }

    fn cl(&self) -> *mut c::closure {
        self.work.cast()
    }

    /// For a ref of its own - a bio's - and to wait on: the allocator, a
    /// commit's journal flush.
    pub fn closure(&self) -> RefBorrow<'_, Opaque<c::closure>> {
        unsafe { RefBorrow::new(&*self.cl().cast::<Opaque<c::closure>>()) }
    }

    /// The running ref, and every other.
    pub fn nr_remaining(&self) -> u32 {
        self.closure().nr_remaining()
    }

    /// Sleeps until the running ref is the only one.
    pub fn sync(&self) {
        unsafe { c::closure_sync(self.cl()) }
    }

    /// Runs @f, out of @wq or by the last closure_put() if None, once every
    /// other ref is put: C's continue_at().
    pub fn continue_at(self, f: ClosureFn<T>, wq: Option<&Queue>) {
        let cl = self.cl();
        core::mem::forget(self);
        unsafe {
            c::set_closure_fn(cl, closure_fn(f), wq_ptr(wq));
            c::bch2_closure_sub(cl, (c::closure_state_CLOSURE_RUNNING + 1) as core::ffi::c_int);
        }
    }

    /// Runs @f now - out of @wq, or called directly if None - without waiting
    /// for the other refs: C's continue_at_nobarrier(). The running ref is
    /// @f's.
    pub fn continue_at_nobarrier(self, f: ClosureFn<T>, wq: Option<&Queue>) {
        let cl = self.cl();
        core::mem::forget(self);
        unsafe {
            c::set_closure_fn(cl, closure_fn(f), wq_ptr(wq));
            c::closure_queue(cl);
        }
    }

    /// Done, the running ref never put: T is the caller's, to free.
    pub fn end(self) -> NonNull<T> {
        unsafe { c::closure_debug_destroy(self.cl()) };
        self.into_raw()
    }
}

impl<T: HasClosure> Deref for Running<T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { self.this().as_ref() }
    }
}

impl<T: HasClosure> DerefMut for Running<T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { self.this().as_mut() }
    }
}

impl<T: HasClosure> Drop for Running<T> {
    fn drop(&mut self) {
        panic!("{}: a closure fn returned holding its running ref", core::any::type_name::<T>());
    }
}

/// Runs @f out of @this's closure, initialized now, as Running: C's
/// closure_call().
///
/// # Safety
/// @this's closure isn't in use, and @this lives until a closure fn end()s it.
pub unsafe fn call<T: HasClosure>(this: NonNull<T>, f: ClosureFn<T>, wq: Option<&Queue>) {
    let cl = this.as_ptr().wrapping_byte_add(T::CLOSURE).cast::<c::closure>();
    unsafe { c::closure_call(cl, closure_fn(f), wq_ptr(wq), core::ptr::null_mut()) }
}
