// SPDX-License-Identifier: GPL-2.0

//! A count of things in flight, each folding what it produced into one
//! result, and a wait for the last: what a closure is in C for its refs -
//! closure_get() before each bio, closure_put() in its endio, closure_sync()
//! - without the continuation, which is the executor's: a task awaits the
//! Gather. A fork-join's jobs folding their errors, first one wins; a flush's
//! shards summing their counters; () for a barrier on IOs that free
//! themselves; or a KVVec of what each put, taken as they come - reaped.
//!
//!     stack_pin_init!(let g = Gather::<Result<(), BchError>, waiter::Sync>::new());
//!     let r = g.get();                // one more in flight
//!     r.put(ret);                     // folded in; dropped unput, nothing is
//!     let ret = g.wait_sync();        // an Async Gather: g.wait(at).await
//!
//! It's waited on by & - everything in it is atomic - so one in something
//! shared is waited on by whoever's job that is: a scope's jobs, by the
//! scope (util/par.rs). One wait at a time, as there's one Done: a second,
//! while one's under way, is a bug, and panics. What's in flight
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
//! No lock: what's put folds into atomics - Fold, a trait on the result -
//! from any context, an endio's included, and the count and whether the
//! owner has waited are one word, changed by cmpxchg, as closure_sub()
//! changes a closure's.
//!
//! A Gather<KVVec<T>> collects what's put instead: each put is pushed,
//! under an irqsave spinlock, onto a vec that has room for it. A put can't
//! allocate, so the owner makes the room, getting each ref by try_get() -
//! which can fail, where get() can't; get() on one doesn't build. The owner
//! take()s what's been put so far while refs are still out, without waiting
//! for the rest: reaping.
//!
//! The wait is a Completion on the Gather's Done, of its waiter kind: Sync's,
//! wait_sync(), sleeping as closure_sync() does, or Async's, wait(), awaited. The wait starts it, then drops the owner's ref; whichever
//! takes the count to zero - that, or the last put - completes it. The owner
//! waits on the Done, not the count, so the Gather is there until complete()
//! has stored DONE, its last touch: the putter's, from its context - an
//! endio's, interrupt context perhaps. A Gather dropped unwaited drops the
//! owner's ref without starting it, and polls the count: the last put's
//! cmpxchg is then its last touch.

use crate::c;
use crate::errcode::{BchError, FirstErr};
use crate::locking::Lock;
use crate::new_lock;
use crate::util::alloc::{flags::GFP_KERNEL, AllocError, KVVec};
use crate::util::sleep::{sleep_until, Sleep};
use crate::util::task::{waiter, Completion, Done, Resumable, Resume, Waiter};
use core::cell::UnsafeCell;
use core::marker::{PhantomData, PhantomPinned};
use core::mem::ManuallyDrop;
use core::pin::Pin;
use core::ptr::{null_mut, NonNull};
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering};
use pin_init::{pin_data, pin_init, PinInit};

/// A Gather's result, and how what its refs put is folded into it:
/// concurrently, from any context - an endio's - so into atomics.
pub trait Fold: Sized {
    /// What a ref puts.
    type Item;
    /// Where the puts fold in.
    type Acc: Sync;

    /// Puts need room made for them, before each ref's got: try_get().
    const RESERVES: bool = false;

    /// The accumulator, as it is with nothing put.
    fn acc() -> impl PinInit<Self::Acc>;
    fn fold(acc: &Self::Acc, v: Self::Item);
    /// The result, once every ref is put - as often as it's asked for.
    fn result(acc: &Self::Acc) -> Self;

    /// Room for @refs more puts than have been: from the owner, who alone
    /// makes room, so it's not racing another's.
    fn reserve(_acc: &Self::Acc, _refs: usize) -> Result<(), AllocError> {
        Ok(())
    }
}

/// Nothing to fold: a barrier.
impl Fold for () {
    type Item = ();
    type Acc = ();

    fn acc() -> impl PinInit<()> {}
    fn fold(_: &(), _: ()) {}
    fn result(_: &()) {}
}

/// The first error put, or Ok.
impl Fold for Result<(), BchError> {
    type Item = Self;
    type Acc = FirstErr;

    fn acc() -> impl PinInit<FirstErr> {
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

/// What each ref put, in the order put. The result is what's been put since
/// it was last asked for - as Gather::take()'s, which has it while refs are
/// still out.
///
/// Its room: a put pushes within capacity, and only the owner grows it - in
/// reserve(), outside the lock, by a new vec it moves the puts into - so
/// capacity is kept at least the puts so far plus the refs out. A ref
/// counted when room's made either has put - in the vec - or hasn't, as a
/// put pushes before its ref's dropped from the count.
impl<T: Send> Fold for KVVec<T> {
    type Item = T;
    type Acc = Lock<c::spinlock_t, KVVec<T>>;

    const RESERVES: bool = true;

    fn acc() -> impl PinInit<Self::Acc> {
        new_lock!(KVVec::new())
    }

    fn fold(acc: &Self::Acc, v: T) {
        let pushed = acc.lock().push_within_capacity(v).is_ok();
        assert!(pushed, "Gather<KVVec> put with no room: a ref not from try_get()");
    }

    fn result(acc: &Self::Acc) -> Self {
        core::mem::take(&mut *acc.lock())
    }

    fn reserve(acc: &Self::Acc, refs: usize) -> Result<(), AllocError> {
        let (len, cap) = {
            let v = acc.lock();
            (v.len(), v.capacity())
        };
        if len + refs <= cap {
            return Ok(());
        }

        let mut new = KVVec::with_capacity((len + refs).max(cap * 2), GFP_KERNEL)?;
        let mut v = acc.lock();
        for x in v.drain_all() {
            // puts since are no more than the refs that were out: they fit
            let pushed = new.push_within_capacity(x).is_ok();
            debug_assert!(pushed);
        }
        core::mem::swap(&mut *v, &mut new);
        drop(v);
        // the old buffer freed with the lock dropped
        Ok(())
    }
}

/// The width of a Gather's state word: its low half counts refs, and above
/// that are two flags. usize by default; u32 to save space, where 2^16 refs
/// will do; u64 for 2^32 refs on 32 bit targets that have 64 bit atomics.
/// Worked in as u64.
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
pub struct Gather<T: Fold, Wt: Waiter, W: Width = usize> {
    state:  W::Atomic,
    /// Born in the wait, with its waiter
    done:   UnsafeCell<Option<Done<Wt>>>,
    #[pin]
    acc:    T::Acc,
    #[pin]
    _pin:   PhantomPinned,
}

// SAFETY: @done is written once, by the wait, before WAITED publishes it to
// the last put; nothing reads it before
unsafe impl<T: Fold, Wt: Waiter, W: Width> Sync for Gather<T, Wt, W> {}

impl<T: Fold, Wt: Waiter, W: Width> Gather<T, Wt, W> {
    /* The state word: */
    /// Refs not put: the owner's, until it waits, and every GatherRef's.
    const COUNT:   u64 = (1 << (W::BITS / 2)) - 1;
    /// The owner has waited: its ref is dropped, and the Done born.
    const WAITED:  u64 = 1 << (W::BITS / 2);
    /// A wait is under way, and the Done is its: one at a time.
    const IN_WAIT: u64 = Self::WAITED << 1;

    pub fn new() -> impl PinInit<Self> {
        pin_init!(Self {
            state:  W::new(1),
            done:   UnsafeCell::new(None),
            acc     <- T::acc(),
            _pin:   PhantomPinned,
        })
    }

    /// A ref: one more thing in flight - from the owner, before it waits.
    pub fn get(&self) -> GatherRef<T, Wt, W> {
        const { assert!(!T::RESERVES, "this Gather's refs are try_get()") };
        self.owner_get()
    }

    /// A ref, with room made for its put: for a Fold that RESERVES.
    pub fn try_get(&self) -> Result<GatherRef<T, Wt, W>, AllocError> {
        let state = W::load(&self.state, Ordering::Relaxed);
        Self::assert_owner(state);
        // the owner's ref counted too: one put it won't make, room to spare
        T::reserve(&self.acc, (state & Self::COUNT) as usize + 1)?;
        Ok(self.get_ref())
    }

    fn owner_get(&self) -> GatherRef<T, Wt, W> {
        Self::assert_owner(W::load(&self.state, Ordering::Relaxed));
        self.get_ref()
    }

    fn assert_owner(state: u64) {
        assert!(state & Self::WAITED == 0, "Gather::get() after its owner waited: get from a ref");
    }

    /// A ref, from someone holding one.
    fn get_ref(&self) -> GatherRef<T, Wt, W> {
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

    /// Fold @v in, if there's one, and drop a ref: the last completes the
    /// Done.
    fn put(&self, v: Option<T::Item>) {
        if let Some(v) = v {
            T::fold(&self.acc, v);
        }

        let mut old = W::load(&self.state, Ordering::Acquire);
        loop {
            assert!(old & Self::COUNT != 0, "Gather put without a ref");
            match W::compare_exchange_weak(&self.state, old, old - 1) {
                Ok(_)    => break,
                Err(cur) => old = cur,
            }
        }
        if (old - 1) & (Self::COUNT|Self::WAITED) == Self::WAITED {
            self.done().complete();
        }
    }

    /// Its Done, once WAITED's been seen.
    fn done(&self) -> &Done<Wt> {
        unsafe { (*self.done.get()).as_ref() }.expect("a Gather's Done, before its wait")
    }

    /// A wait begun: the Done ours, till it's dropped.
    fn begin_wait(&self) -> InWait<'_, T, Wt, W> {
        // checked before it's changed: a panic leaves the word as it was
        let mut old = W::load(&self.state, Ordering::Relaxed);
        loop {
            assert!(old & Self::IN_WAIT == 0, "Gather waited on twice at once");
            match W::compare_exchange_weak(&self.state, old, old | Self::IN_WAIT) {
                Ok(_)    => return InWait(self),
                Err(cur) => old = cur,
            }
        }
    }

    /// Waited on before: that wait is over, as a Completion's drop waits it
    /// out.
    fn waited(&self) -> bool {
        W::load(&self.state, Ordering::Acquire) & Self::WAITED != 0
    }

    /// The wait's Completion: the Done born, with @waiter, then the owner's ref
    /// dropped - WAITED publishes the Done to the last put.
    fn start(&self, waiter: Wt) -> Completion<'_, Wt> {
        // SAFETY: written once, here, before WAITED; nothing reads it till then
        let c = Completion::new(unsafe { &mut *self.done.get() }, waiter);

        let mut old = W::load(&self.state, Ordering::Acquire);
        while let Err(cur) = W::compare_exchange_weak(&self.state, old, (old - 1) | Self::WAITED) {
            old = cur;
        }
        if (old - 1) & Self::COUNT == 0 {
            self.done().complete();
        }
        c
    }
}

struct InWait<'a, T: Fold, Wt: Waiter, W: Width>(&'a Gather<T, Wt, W>);

impl<T: Fold, Wt: Waiter, W: Width> Drop for InWait<'_, T, Wt, W> {
    fn drop(&mut self) {
        let state = &self.0.state;
        let mut old = W::load(state, Ordering::Relaxed);
        while let Err(cur) = W::compare_exchange_weak(state, old, old & !Gather::<T, Wt, W>::IN_WAIT) {
            old = cur;
        }
    }
}

impl<T: Fold, W: Width> Gather<T, waiter::Sync, W> {
    /// Wait for every ref to be put, sleeping TASK_IDLE - no hung task
    /// warnings - and take the result.
    pub fn wait_sync(&self) -> T {
        self.wait_sync_state(Sleep::IDLE)
    }

    /// wait_sync(), freezable: for refs whose holders freeze in place - kthreads
    /// that do - whose owner must too.
    pub fn wait_sync_freezable(&self) -> T {
        self.wait_sync_state(Sleep::IDLE.freezable())
    }

    fn wait_sync_state(&self, how: Sleep) -> T {
        let _w = self.begin_wait();
        if !self.waited() {
            self.start(waiter::Sync::current()).wait(how);
        }
        T::result(&self.acc)
    }
}

impl<T: Fold, W: Width> Gather<T, waiter::Async, W> {
    /// Wait for every ref to be put, resumed @at, for the result.
    pub async fn wait(&self, at: Resume<'_>) -> T {
        let _w = self.begin_wait();
        if !self.waited() {
            self.start(waiter::Async::current().await).resume_on(at).await;
        }
        T::result(&self.acc)
    }
}

impl<T: Send, Wt: Waiter, W: Width> Gather<KVVec<T>, Wt, W> {
    /// What's been put since the last take, in @out - emptied first - with
    /// refs still out: the puts move into the buffer @out had, which is
    /// given the room theirs had, so any yet to come still fit.
    pub fn take(&self, out: &mut KVVec<T>) -> Result<(), AllocError> {
        out.clear();
        // only the owner - us - changes it
        let cap = self.acc.lock().capacity();
        if out.capacity() < cap {
            out.reserve(cap, GFP_KERNEL)?;
        }
        core::mem::swap(&mut *self.acc.lock(), out);
        Ok(())
    }
}

#[pin_init::pinned_drop]
impl<T: Fold, Wt: Waiter, W: Width> PinnedDrop for Gather<T, Wt, W> {
    fn drop(self: Pin<&mut Self>) {
        // Not waited on - its owner returned early - and refs may be out, raw
        // ones in C's hands: wait them out. It's pinned, so it's where they
        // point until it's freed.
        if !self.waited() {
            let mut old = W::load(&self.state, Ordering::Acquire);
            while let Err(cur) = W::compare_exchange_weak(&self.state, old, old - 1) {
                old = cur;
            }
            let all_put = || (W::load(&self.state, Ordering::Acquire) & Self::COUNT == 0).then_some(());
            while all_put().is_none() {
                let _ = sleep_until(Sleep::IDLE.timeout(1), all_put);
            }
        }
    }
}

/// A ref on a Gather: put with a value, or dropped without one.
#[must_use = "a ref is put as soon as it's dropped"]
pub struct GatherRef<T: Fold, Wt: Waiter, W: Width = usize>(NonNull<Gather<T, Wt, W>>);

// SAFETY: a Gather is atomics, and stays where it is until every ref is put;
// what's put crosses to the owner's thread, so T::Item: Send
unsafe impl<T: Fold, Wt: Waiter, W: Width> Send for GatherRef<T, Wt, W> where T::Item: Send {}
unsafe impl<T: Fold, Wt: Waiter, W: Width> Sync for GatherRef<T, Wt, W> where T::Item: Send {}

impl<T: Fold, Wt: Waiter, W: Width> GatherRef<T, Wt, W> {
    fn gather(&self) -> &Gather<T, Wt, W> {
        // SAFETY: a ref holds the Gather where it is: its Drop waits for it
        unsafe { self.0.as_ref() }
    }

    /// Another ref, for something this one's holder starts.
    pub fn get(&self) -> GatherRef<T, Wt, W> {
        const { assert!(!T::RESERVES, "this Gather's refs are try_get(), by its owner") };
        self.gather().get_ref()
    }

    /// Fold @v into the result, and drop the ref.
    pub fn put(self, v: T::Item) {
        let g = ManuallyDrop::new(self);
        g.gather().put(Some(v));
    }

    /// The ref as a pointer, for C - an endio's private - to put with
    /// from_raw().put().
    pub fn into_raw(self) -> *const Gather<T, Wt, W> {
        ManuallyDrop::new(self).0.as_ptr()
    }

    /// The ref into_raw() made.
    ///
    /// # Safety
    /// @g is from into_raw(), and this is the one from_raw() of it.
    pub unsafe fn from_raw(g: *const Gather<T, Wt, W>) -> Self {
        GatherRef(NonNull::new(g.cast_mut()).unwrap())
    }
}

impl<T: Fold, Wt: Waiter, W: Width> Drop for GatherRef<T, Wt, W> {
    fn drop(&mut self) {
        self.gather().put(None);
    }
}

/// A GatherRef with no one holder - a count's, put by the count's last put,
/// from any context: so taken through &, once, by whoever takes it first.
/// Send and Sync as the ref is.
pub struct GatherRefSlot<T: Fold, Wt: Waiter, W: Width = usize> {
    r:  AtomicPtr<Gather<T, Wt, W>>,
    _r: PhantomData<GatherRef<T, Wt, W>>,
}

impl<T: Fold, Wt: Waiter, W: Width> GatherRefSlot<T, Wt, W> {
    pub const fn new() -> Self {
        GatherRefSlot { r: AtomicPtr::new(null_mut()), _r: PhantomData }
    }

    /// @r, to hold till it's taken: the slot's empty.
    pub fn set(&self, r: GatherRef<T, Wt, W>) {
        let p = r.into_raw().cast_mut();
        let old = self.r.swap(p, Ordering::AcqRel);
        assert!(old.is_null(), "a GatherRefSlot set twice");
    }

    /// Its ref, if it hasn't been taken.
    pub fn take(&self) -> Option<GatherRef<T, Wt, W>> {
        let p = self.r.swap(null_mut(), Ordering::AcqRel);
        // SAFETY: set()'s into_raw(), swapped out once: ours alone
        (!p.is_null()).then(|| unsafe { GatherRef::from_raw(p) })
    }
}

impl<T: Fold, Wt: Waiter, W: Width> Drop for GatherRefSlot<T, Wt, W> {
    fn drop(&mut self) {
        drop(self.take());
    }
}
