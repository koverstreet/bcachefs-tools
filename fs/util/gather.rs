// SPDX-License-Identifier: GPL-2.0

//! A count of things in flight, each folding what it produced into one
//! result, and a wait for the last: what a closure is in C for its refs -
//! closure_get() before each bio, closure_put() in its endio, closure_sync()
//! - without the continuation, which is the executor's: a task awaits the
//! Gather. A fork-join's jobs folding their errors, first one wins; a flush's
//! shards summing their counters; or () for a barrier on IOs that free
//! themselves.
//!
//!     stack_pin_init!(let g = Gather::<Result<(), BchError>>::new());
//!     let r = g.get();                // one more in flight
//!     r.put(ret);                     // folded in; dropped unput, nothing is
//!     let ret = g.wait_sync();        // or g.wait().resume_on(..).await
//!
//! It's waited on by & - everything in it is atomic - so one in something
//! shared is waited on by whoever's job that is: a task's, by its
//! JoinHandle. One wait at a time, as there's one slot for the waiter: a
//! second, while one's under way, is a bug, and panics. What's in flight
//! holds refs - GatherRef, Send and Sync - got from the owner before it
//! waits, or from a ref. Made, a Gather holds one ref of its own, the
//! owner's, as closure_init()'s; the wait drops it, so the count only
//! reaches zero once the owner waits, and never goes from zero back up.
//!
//! A ref has no lifetime. What keeps the Gather there for it is that a
//! Gather is only ever made pinned (PinInit), so its Drop runs before its
//! memory is reused - and its Drop waits for every ref outstanding. So a ref
//! can go anywhere - to C, an endio's private pointer, through into_raw() -
//! and the owner's frame waits for it. Await or wait before it's dropped:
//! that Drop sleeps.
//!
//! No lock: what's put folds into atomics - Fold, a trait on what's put -
//! from any context, an endio's included, and the count, whether the owner
//! has waited and whether a waiter is registered are one word, changed by
//! cmpxchg, as closure_sub() changes a closure's.
//!
//! The waiter is a Waker in a slot, as raw parts: an await's, or a sync
//! wait's - which sleeps as closure_sync() does - waking its task, with a ref
//! on it. The waiter fills it, then sets WAITING; the last put reads it, then
//! clears WAITING with the cmpxchg that takes the count to zero. That
//! cmpxchg is the putter's last touch of the Gather - after it the owner may
//! see zero, return and free it - and gives the putter the Waker: it wakes
//! it, holding nothing of the Gather's.
//!
//! An await polled with a new Waker takes its old one back - clearing
//! WAITING - and refills the slot, setting WAITING again and bumping a
//! generation in the same word: a put that read the slot before the refill
//! fails its cmpxchg, rather than taking a slot that changed under it. An
//! await dropped takes its Waker back the same way. One forgotten keeps the
//! wait - another panics - till the Gather's drop takes both back.
//!
//! The Waker is woken in the last put's context - an endio's, interrupt
//! context perhaps - so its wake must be safe there: the executor's is.

use crate::errcode::{BchError, FirstErr};
use crate::util::sleep::{sleep_until, task_waker, Sleep};
use crate::util::task::Resumable;
use core::marker::{PhantomData, PhantomPinned};
use core::mem::ManuallyDrop;
use core::pin::Pin;
use core::ptr::{null_mut, NonNull};
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering};
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use pin_init::{pin_data, pin_init, PinInit};

/// What a Gather's refs put, and how it's folded into one result:
/// concurrently, from any context - an endio's - so into atomics.
pub trait Fold: Sized {
    /// Where the puts fold in.
    type Acc: Sync;

    /// The accumulator, as it is with nothing put.
    fn acc() -> Self::Acc;
    fn fold(acc: &Self::Acc, v: Self);
    /// The result, once every ref is put - as often as it's asked for.
    fn result(acc: &Self::Acc) -> Self;
}

/// Nothing to fold: a barrier.
impl Fold for () {
    type Acc = ();

    fn acc() {}
    fn fold(_: &(), _: ()) {}
    fn result(_: &()) {}
}

/// The first error put, or Ok.
impl Fold for Result<(), BchError> {
    type Acc = FirstErr;

    fn acc() -> FirstErr {
        FirstErr::new()
    }

    fn fold(acc: &FirstErr, v: Self) {
        if let Err(e) = v {
            acc.set(e);
        }
    }

    fn result(acc: &FirstErr) -> Self {
        acc.result()
    }
}

/// The width of a Gather's state word: its low half counts refs, and above
/// that are three flags and a generation. usize by default; u32 to save
/// space, where 2^16 refs and a 13 bit generation will do; u64 for 2^32
/// refs on 32 bit targets that have 64 bit atomics. Worked in as u64.
pub trait Width: sealed::Sealed {
    type Atomic: Send + Sync;
    const BITS: u32;

    fn new(v: u64) -> Self::Atomic;
    fn load(a: &Self::Atomic, order: Ordering) -> u64;
    /// AcqRel, Acquire on failure: every change to the word orders what's
    /// around it.
    fn compare_exchange_weak(a: &Self::Atomic, old: u64, new: u64) -> Result<u64, u64>;
}

mod sealed {
    pub trait Sealed {}
}

macro_rules! width {
    ($t:ty, $atomic:ty) => {
        impl sealed::Sealed for $t {}

        impl Width for $t {
            type Atomic = $atomic;
            const BITS: u32 = <$t>::BITS;

            fn new(v: u64) -> $atomic {
                <$atomic>::new(v as $t)
            }

            fn load(a: &$atomic, order: Ordering) -> u64 {
                a.load(order) as u64
            }

            fn compare_exchange_weak(a: &$atomic, old: u64, new: u64) -> Result<u64, u64> {
                a.compare_exchange_weak(old as $t, new as $t, Ordering::AcqRel, Ordering::Acquire)
                    .map(|v| v as u64)
                    .map_err(|v| v as u64)
            }
        }
    };
}

width!(u32, AtomicU32);
#[cfg(target_has_atomic = "64")]
width!(u64, core::sync::atomic::AtomicU64);
width!(usize, AtomicUsize);

/// Everything in it is atomic: refs reach it through a pointer, and its
/// waiter by &.
#[pin_data(PinnedDrop)]
pub struct Gather<T: Fold, W: Width = usize> {
    state:  W::Atomic,
    waiter: Slot,
    acc:    T::Acc,
    #[pin]
    _pin:   PhantomPinned,
}

/// The waiter's Waker, as raw parts, for the last put: written by the waiter
/// only with WAITING clear, read by a put only after seeing it set.
struct Slot {
    data:   AtomicPtr<()>,
    vtable: AtomicPtr<RawWakerVTable>,
}

type RawParts = (*mut (), *mut RawWakerVTable);

impl Slot {
    const fn new() -> Self {
        Slot { data: AtomicPtr::new(null_mut()), vtable: AtomicPtr::new(null_mut()) }
    }

    fn read(&self) -> RawParts {
        (self.data.load(Ordering::Relaxed), self.vtable.load(Ordering::Relaxed))
    }

    /// @w given up to the slot: its parts, which own it.
    fn write(&self, w: Waker) -> RawParts {
        let w = ManuallyDrop::new(w);
        let (data, vtable) = (w.data().cast_mut(), core::ptr::from_ref(w.vtable()).cast_mut());
        self.data.store(data, Ordering::Relaxed);
        self.vtable.store(vtable, Ordering::Relaxed);
        (data, vtable)
    }

    fn will_wake(&self, w: &Waker) -> bool {
        self.read() == (w.data().cast_mut(), core::ptr::from_ref(w.vtable()).cast_mut())
    }
}

/// # Safety
/// From Slot::write(), and owned: taken once.
unsafe fn waker_from((data, vtable): RawParts) -> Waker {
    // SAFETY: the caller's
    unsafe { Waker::from_raw(RawWaker::new(data, &*vtable)) }
}

impl<T: Fold, W: Width> Gather<T, W> {
    /* The state word: */
    /// Refs not put: the owner's, until it waits, and every GatherRef's.
    const COUNT:   u64 = (1 << (W::BITS / 2)) - 1;
    /// The slot holds a waiter: the last put takes it.
    const WAITING: u64 = 1 << (W::BITS / 2);
    /// The owner has waited: its ref is dropped.
    const WAITED:  u64 = Self::WAITING << 1;
    /// A wait is under way - a Wait, or a sync wait - and the slot is its:
    /// one at a time.
    const IN_WAIT: u64 = Self::WAITING << 2;
    /// Bumped each time the slot is filled; wraps out of the word's top.
    const GEN:     u64 = Self::WAITING << 3;

    pub fn new() -> impl PinInit<Self> {
        pin_init!(Self {
            state:  W::new(1),
            waiter: Slot::new(),
            acc:    T::acc(),
            _pin:   PhantomPinned,
        })
    }

    /// A ref: one more thing in flight - from the owner, before it waits.
    pub fn get(&self) -> GatherRef<T, W> {
        assert!(W::load(&self.state, Ordering::Relaxed) & Self::WAITED == 0,
                "Gather::get() after its owner waited: get from a ref");
        self.get_ref()
    }

    /// A ref, from someone holding one.
    fn get_ref(&self) -> GatherRef<T, W> {
        // checked before it's changed: a panic leaves the word as it was
        let mut old = W::load(&self.state, Ordering::Relaxed);
        loop {
            assert!(old & Self::COUNT != 0, "Gather ref got with none held");
            assert!(old & Self::COUNT != Self::COUNT, "Gather: too many refs");
            match W::compare_exchange_weak(&self.state, old, old + 1) {
                Ok(_)    => return GatherRef(NonNull::from(self)),
                Err(cur) => old = cur,
            }
        }
    }

    /// Fold @v in, if there's one, and drop a ref: the last wakes the waiter.
    fn put(&self, v: Option<T>) {
        if let Some(v) = v {
            T::fold(&self.acc, v);
        }

        let mut old = W::load(&self.state, Ordering::Acquire);
        loop {
            assert!(old & Self::COUNT != 0, "Gather put without a ref");
            let mut new = old - 1;
            // raw parts, the waiter's if the cmpxchg is the last put's
            let mut waiter = None;
            if new & (Self::COUNT|Self::WAITING) == Self::WAITING {
                new &= !Self::WAITING;
                waiter = Some(self.waiter.read());
            }
            match W::compare_exchange_weak(&self.state, old, new) {
                Ok(_) => {
                    // the Gather may be gone: what's ours is the waiter
                    if let Some(w) = waiter {
                        // SAFETY: read with WAITING set, unchanged since: GEN
                        unsafe { waker_from(w) }.wake();
                    }
                    return;
                }
                Err(cur) => old = cur,
            }
        }
    }

    /// A wait begun: the slot ours, till end_wait().
    fn begin_wait(&self) {
        // checked before it's changed: a panic leaves the word as it was
        let mut old = W::load(&self.state, Ordering::Relaxed);
        loop {
            assert!(old & Self::IN_WAIT == 0, "Gather waited on twice at once");
            match W::compare_exchange_weak(&self.state, old, old | Self::IN_WAIT) {
                Ok(_)    => return,
                Err(cur) => old = cur,
            }
        }
    }

    fn end_wait(&self) {
        let mut old = W::load(&self.state, Ordering::Relaxed);
        while let Err(cur) = W::compare_exchange_weak(&self.state, old, old & !Self::IN_WAIT) {
            old = cur;
        }
    }

    /// The owner's ref dropped, the first time; whether every ref's put.
    fn start_wait(&self) -> bool {
        let mut old = W::load(&self.state, Ordering::Acquire);
        while old & Self::WAITED == 0 {
            match W::compare_exchange_weak(&self.state, old, (old - 1) | Self::WAITED) {
                Ok(_)    => old = (old - 1) | Self::WAITED,
                Err(cur) => old = cur,
            }
        }
        old & Self::COUNT == 0
    }

    /// Every ref put, the owner's too.
    fn done(&self) -> bool {
        W::load(&self.state, Ordering::Acquire) & (Self::WAITED|Self::COUNT) == Self::WAITED
    }

    /// Leave @w for the last put, the owner having waited - or have it
    /// back, if every ref's been put. A waiter still left - a Wait forgotten
    /// rather than dropped - is taken back first: the slot's only written
    /// with WAITING clear.
    fn register(&self, w: Waker) -> Result<(), Waker> {
        self.unregister();

        let raw = self.waiter.write(w);

        let mut old = W::load(&self.state, Ordering::Acquire);
        loop {
            debug_assert!(old & (Self::WAITED|Self::WAITING) == Self::WAITED);
            if old & Self::COUNT == 0 {
                // SAFETY: never published
                return Err(unsafe { waker_from(raw) });
            }
            let new = (old | Self::WAITING).wrapping_add(Self::GEN);
            match W::compare_exchange_weak(&self.state, old, new) {
                Ok(_)    => return Ok(()),
                Err(cur) => old = cur,
            }
        }
    }

    /// Take back the waiter left - unless the last put has: then it's done.
    fn unregister(&self) -> bool {
        let mut old = W::load(&self.state, Ordering::Acquire);
        while old & Self::WAITING != 0 {
            match W::compare_exchange_weak(&self.state, old, old & !Self::WAITING) {
                Ok(_) => {
                    // SAFETY: ours again: WAITING is clear
                    drop(unsafe { waker_from(self.waiter.read()) });
                    return true;
                }
                Err(cur) => old = cur,
            }
        }
        false
    }

    /// Wait for every ref to be put, sleeping TASK_IDLE - no hung task
    /// warnings - and take the result.
    pub fn wait_sync(&self) -> T {
        self.wait_sync_state(false)
    }

    /// wait_sync(), freezable: for refs whose holders freeze in place - kthreads
    /// that do - whose owner must too.
    pub fn wait_sync_freezable(&self) -> T {
        self.wait_sync_state(true)
    }

    fn wait_sync_state(&self, freezable: bool) -> T {
        self.begin_wait();
        if !self.start_wait() && self.register(task_waker()).is_ok() {
            let how = if freezable { Sleep::IDLE.freezable() } else { Sleep::IDLE };
            let Ok(()) = sleep_until(how, || self.done().then_some(()));
        }
        self.end_wait();
        T::result(&self.acc)
    }

    /// Wait for every ref to be put, asynchronously, for the result.
    pub fn wait(&self) -> Wait<'_, T, W> {
        self.begin_wait();
        Wait { g: self, registered: false }
    }
}

#[pin_init::pinned_drop]
impl<T: Fold, W: Width> PinnedDrop for Gather<T, W> {
    fn drop(self: Pin<&mut Self>) {
        // Not waited on - its owner returned early, or an await of it was
        // dropped - and refs may be out, raw ones in C's hands: wait them
        // out. It's pinned, so it's where they point until it's freed. A
        // Wait forgotten still has the wait: nothing waits on what's being
        // dropped, so it's ours.
        if !self.done() {
            self.end_wait();
            self.wait_sync_state(false);
        }
    }
}

/// A ref on a Gather: put with a value, or dropped without one.
#[must_use = "a ref is put as soon as it's dropped"]
pub struct GatherRef<T: Fold, W: Width = usize>(NonNull<Gather<T, W>>);

// SAFETY: a Gather is atomics, and stays where it is until every ref is put;
// what's put crosses to the owner's thread, so T: Send
unsafe impl<T: Fold + Send, W: Width> Send for GatherRef<T, W> {}
unsafe impl<T: Fold + Send, W: Width> Sync for GatherRef<T, W> {}

impl<T: Fold, W: Width> GatherRef<T, W> {
    fn gather(&self) -> &Gather<T, W> {
        // SAFETY: a ref holds the Gather where it is: its Drop waits for it
        unsafe { self.0.as_ref() }
    }

    /// Another ref, for something this one's holder starts.
    pub fn get(&self) -> GatherRef<T, W> {
        self.gather().get_ref()
    }

    /// Fold @v into the result, and drop the ref.
    pub fn put(self, v: T) {
        let g = ManuallyDrop::new(self);
        g.gather().put(Some(v));
    }

    /// The ref as a pointer, for C - an endio's private - to put with
    /// from_raw().put().
    pub fn into_raw(self) -> *const Gather<T, W> {
        ManuallyDrop::new(self).0.as_ptr()
    }

    /// The ref into_raw() made.
    ///
    /// # Safety
    /// @g is from into_raw(), and this is the one from_raw() of it.
    pub unsafe fn from_raw(g: *const Gather<T, W>) -> Self {
        GatherRef(NonNull::new(g.cast_mut()).unwrap())
    }
}

impl<T: Fold, W: Width> Drop for GatherRef<T, W> {
    fn drop(&mut self) {
        self.gather().put(None);
    }
}

/// A GatherRef with no one holder - a count's, put by the count's last put,
/// from any context: so taken through &, once, by whoever takes it first.
/// Send and Sync as the ref is.
pub struct GatherRefSlot<T: Fold, W: Width = usize> {
    r:  AtomicPtr<Gather<T, W>>,
    _r: PhantomData<GatherRef<T, W>>,
}

impl<T: Fold, W: Width> GatherRefSlot<T, W> {
    pub const fn new() -> Self {
        GatherRefSlot { r: AtomicPtr::new(null_mut()), _r: PhantomData }
    }

    /// @r, to hold till it's taken: the slot's empty.
    pub fn set(&self, r: GatherRef<T, W>) {
        let p = r.into_raw().cast_mut();
        let old = self.r.swap(p, Ordering::AcqRel);
        assert!(old.is_null(), "a GatherRefSlot set twice");
    }

    /// Its ref, if it hasn't been taken.
    pub fn take(&self) -> Option<GatherRef<T, W>> {
        let p = self.r.swap(null_mut(), Ordering::AcqRel);
        // SAFETY: set()'s into_raw(), swapped out once: ours alone
        (!p.is_null()).then(|| unsafe { GatherRef::from_raw(p) })
    }
}

impl<T: Fold, W: Width> Drop for GatherRefSlot<T, W> {
    fn drop(&mut self) {
        drop(self.take());
    }
}

/// Gather::wait()'s: the result, once every ref is put - awaited through
/// resume_on(). Dropped unfinished, it takes its Waker back.
#[must_use = "a wait does nothing unless awaited"]
pub struct Wait<'a, T: Fold, W: Width = usize> {
    g:          &'a Gather<T, W>,
    /// Our Waker is in the slot.
    registered: bool,
}

impl<T: Fold, W: Width> Resumable for Wait<'_, T, W> {
    type Output = T;

    fn poll_resume(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let this = self.get_mut();
        let g = this.g;
        if g.start_wait() {
            return Poll::Ready(T::result(&g.acc));
        }
        // still left, and as it would be: the slot's only ours to change
        if this.registered && g.waiter.will_wake(cx.waker()) {
            return Poll::Pending;
        }
        this.registered = false;
        match g.register(cx.waker().clone()) {
            Ok(()) => {
                this.registered = true;
                Poll::Pending
            }
            Err(_w) => Poll::Ready(T::result(&g.acc)),
        }
    }
}

impl<T: Fold, W: Width> Drop for Wait<'_, T, W> {
    fn drop(&mut self) {
        if self.registered {
            self.g.unregister();
        }
        self.g.end_wait();
    }
}
