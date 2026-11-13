// SPDX-License-Identifier: GPL-2.0

use crate::util::place::{Contained, Field, FieldOf, Here, Path, Place, Placed, RefValue};
use crate::util::refcount::AtomicRef;
use crate::util::projections::Publish;
use core::cell::UnsafeCell;
use core::ffi::{c_ulong, c_void, CStr};
use core::marker::{PhantomData, PhantomPinned};
use core::ops::{Deref, DerefMut};

/// C's lock state, and how it's initialized and torn down - whatever kind of
/// lock it is, which the traits over this one say - implemented on C's lock
/// types themselves, where the kernel crate's lock Backend is a marker over
/// them.
///
/// # Safety
/// All of its state is interior-mutable - C's locks are Opaques, ours atomics
/// - so it's taken and released through a shared reference, from any thread.
/// Callers pass an initialized lock, except to init() - for a C lock, a & to
/// one zeroed isn't one: a zeroed kernel mutex's wait list is null - and
/// release with the state taking it returned.
pub unsafe trait RawLockBase {
    /// What releasing it needs: the interrupt state spin_lock_irqsave()
    /// saved, the memalloc flags a Noio saved, or nothing.
    type GuardState;
    /// init()'s, for a lock that allocates: Infallible otherwise.
    type InitError;

    /// In lockdep class @key, named @name, at @l - uninitialized or zeroed
    /// memory, which nothing else reaches until it returns; pinned from here
    /// on.
    unsafe fn init(l: *mut Self, name: &'static CStr, key: *mut c_void)
        -> Result<(), Self::InitError>;

    /// For a lock that allocates; nothing holds it or will.
    unsafe fn destroy(&self) {}
}

/// A lock taken and released whole, by one holder at a time.
///
/// # Safety
/// lock() excludes every other holder until unlock(); as RawLockBase's
/// otherwise.
pub unsafe trait RawLock: RawLockBase {
    unsafe fn lock(&self) -> Self::GuardState;

    unsafe fn unlock(&self, g: Self::GuardState);

    /// That it's held exclusively - lockdep_assert_held(), and
    /// lockdep_assert_held_write() for a lock readers can share: a reader
    /// holding it isn't enough.
    unsafe fn assert_held(&self);
}

/// A RawLock that can be tried: taken only if no one holds it.
///
/// # Safety
/// try_lock() is RawLock's lock(), when it returns Some; when None, nothing's
/// taken, and nothing's left to release.
pub unsafe trait RawTryLock: RawLock {
    unsafe fn try_lock(&self) -> Option<Self::GuardState>;
}

/// A RawTryLock that can be waited for, for a while.
///
/// # Safety
/// try_lock_timeout() is RawTryLock's try_lock(), but waiting up to @timeout
/// jiffies for it.
pub unsafe trait RawTimedLock: RawTryLock {
    unsafe fn try_lock_timeout(&self, timeout: c_ulong) -> Option<Self::GuardState>;
}

/// A RawLock that can be taken as one of several of its lockdep class held
/// at once - the others' order keeping them apart - each a subclass.
///
/// # Safety
/// lock_nested() is RawLock's lock(), but for what lockdep is told.
pub unsafe trait RawNestedLock: RawLock {
    unsafe fn lock_nested(&self, subclass: u32) -> Self::GuardState;
}

/// A lock readers share, whatever else it has - RawRwLock's writer,
/// RawSixLock's intent and write.
///
/// # Safety
/// lock_read() excludes only what writes, until unlock_read(); as
/// RawLockBase's otherwise.
pub unsafe trait RawReadLock: RawLockBase {
    unsafe fn lock_read(&self) -> Self::GuardState;
    unsafe fn unlock_read(&self, g: Self::GuardState);
}

/// A RawReadLock that can be tried: taken only if no writer holds it.
///
/// # Safety
/// try_lock_read() is lock_read(), when it returns Some; when None, nothing's
/// taken, and nothing's left to release.
pub unsafe trait RawTryReadLock: RawReadLock {
    unsafe fn try_lock_read(&self) -> Option<Self::GuardState>;
}

/// A RawLock that readers can share: RawLock's lock() and unlock() are the
/// writer's, alone.
///
/// # Safety
/// lock() excludes readers too; as RawLock's and RawReadLock's otherwise.
pub unsafe trait RawRwLock: RawLock + RawReadLock {
    /// lockdep_assert_held(): by a reader or the writer.
    unsafe fn assert_held_any(&self);
}

/// A six lock: read; intent, shared with readers but held by one at a time;
/// and write, which only the intent holder takes, excluding readers. Not a
/// RawLock: there's no taking it alone but through intent, which is how an
/// intent holder gets to write without deadlocking against another.
///
/// # Safety
/// The modes exclude as they say. lock_write() is taken only by the intent
/// holder, and released by unlock_write() before its unlock_intent(); as
/// RawReadLock's otherwise.
pub unsafe trait RawSixLock: RawReadLock {
    unsafe fn lock_intent(&self) -> Self::GuardState;
    unsafe fn unlock_intent(&self, g: Self::GuardState);
    unsafe fn lock_write(&self) -> Self::GuardState;
    unsafe fn unlock_write(&self, g: Self::GuardState);
}

/// Two things locked by @R: @G, protected - reached only with it held, its
/// guard derefs to it - and @P, published: anyone's, the lock derefs to it,
/// held or not; written through the guard's published(), so the lock a
/// writer holds is the one it's in.
#[repr(C)]
pub struct Locked<R: RawLockBase, G = (), P = ()> {
    raw:       R,
    protected: UnsafeCell<G>,
    published: P,
    // pinned - a kernel lock mustn't move - in a PhantomData: a bare
    // PhantomPinned isn't FFI-safe in a repr(C) struct, and C's hold these
    _pin:      PhantomData<PhantomPinned>,
}

unsafe impl<R: RawLockBase, G, P> crate::util::place::Wrapper for Locked<R, G, P> {
    type Inner = G;
    const INNER_OFFSET: usize = core::mem::offset_of!(Self, protected);
}

// G is had by one thread at a time, as a std Mutex's; P is every thread's.
// read() shares G, so it wants G: Sync.
unsafe impl<R: RawLockBase, G: Send, P: Send> Send for Locked<R, G, P> {}
unsafe impl<R: RawLockBase, G: Send, P: Sync> Sync for Locked<R, G, P> {}

impl<R: RawLockBase, G, P> Locked<R, G, P> {
    /// new_lock!()'s, which has a static lockdep key per call site.
    ///
    /// # Safety
    /// @key is a lock class key that lives forever - or null, in userspace.
    #[doc(hidden)]
    pub unsafe fn new(g: G, p: P, name: &'static CStr, key: *mut c_void)
        -> impl ::pin_init::PinInit<Self, R::InitError>
    {
        // If init() fails, nothing's written to drop.
        unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut Self| {
                R::init(&raw mut (*slot).raw, name, key)?;
                (&raw mut (*slot).protected).write(UnsafeCell::new(g));
                (&raw mut (*slot).published).write(p);
                Ok(())
            })
        }
    }

    /// new(), with @g initialized in place: for a G too big to build on the
    /// stack and move in - new_lock!(<- init). For a lock whose init() can't
    /// fail, so that nothing's left to undo if @g's does.
    ///
    /// # Safety
    /// As new()'s.
    #[doc(hidden)]
    pub unsafe fn new_init<E>(g: impl ::pin_init::PinInit<G, E>, p: P, name: &'static CStr, key: *mut c_void)
        -> impl ::pin_init::PinInit<Self, E>
    where
        R: RawLockBase<InitError = core::convert::Infallible>,
    {
        unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut Self| {
                let Ok(()) = R::init(&raw mut (*slot).raw, name, key);
                // UnsafeCell<G> is laid out as G
                crate::util::alloc::init_at(g, (&raw mut (*slot).protected).cast::<G>())?;
                (&raw mut (*slot).published).write(p);
                Ok(())
            })
        }
    }

    /// A Locked for a static: @g and @p, the raw lock zeroed - a C lock can't
    /// be made in a const - until init_zeroed().
    ///
    /// # Safety
    /// init_zeroed() runs before anything takes it.
    pub const unsafe fn uninit(g: G, p: P) -> Self {
        Self {
            raw:       unsafe { core::mem::zeroed() },
            protected: UnsafeCell::new(g),
            published: p,
            _pin:      PhantomData,
        }
    }

    /// For a Locked in zeroed memory that new() can't reach - a static, or
    /// uninit()'s: its raw lock initialized in place.
    ///
    /// # Safety
    /// Once, before anything takes it, its G and P valid - zeroed, or
    /// uninit()'s. @key is as new()'s.
    pub unsafe fn init_zeroed(&self, name: &'static CStr, key: *mut c_void) -> Result<(), R::InitError> {
        unsafe { R::init(core::ptr::from_ref(&self.raw).cast_mut(), name, key) }
    }

    /// Where the G is in a Locked: for C's container_of() from it.
    pub const PROTECTED_OFFSET: usize = core::mem::offset_of!(Self, protected);

    pub(super) fn raw(&self) -> &R {
        &self.raw
    }
}

impl<R: RawLock, G, P> Locked<R, G, P> {
    pub fn lock(&self) -> ExclusiveGuard<'_, R, G, P> {
        let state = unsafe { self.raw.lock() };
        ExclusiveGuard { lock: self, state: Some(state), _not_send: PhantomData }
    }

    /// lock(), as lockdep subclass @subclass: for locks of one class taken
    /// together, always in the same order.
    pub fn lock_nested(&self, subclass: u32) -> ExclusiveGuard<'_, R, G, P>
    where
        R: RawNestedLock,
    {
        let state = unsafe { self.raw.lock_nested(subclass) };
        ExclusiveGuard { lock: self, state: Some(state), _not_send: PhantomData }
    }

    /// lock(), if no one holds it.
    pub fn try_lock(&self) -> Option<ExclusiveGuard<'_, R, G, P>>
    where
        R: RawTryLock,
    {
        let state = unsafe { self.raw.try_lock() }?;
        Some(ExclusiveGuard { lock: self, state: Some(state), _not_send: PhantomData })
    }

    /// lock(), if it's free within @timeout jiffies.
    pub fn try_lock_timeout(&self, timeout: c_ulong) -> Option<ExclusiveGuard<'_, R, G, P>>
    where
        R: RawTimedLock,
    {
        let state = unsafe { self.raw.try_lock_timeout(timeout) }?;
        Some(ExclusiveGuard { lock: self, state: Some(state), _not_send: PhantomData })
    }

    /// Held exclusively - by the writer, for a lock readers share.
    pub fn assert_held(&self) {
        unsafe { self.raw.assert_held() }
    }

    /// What it protects, unlocked: for a racy peek, one that can live with
    /// what it reads changing under it.
    pub fn data_ptr(&self) -> *const G {
        self.protected.get()
    }

    /// What it protects, had by &mut - no one else's to lock.
    pub fn get_mut(&mut self) -> &mut G {
        self.protected.get_mut()
    }
}

impl<R: RawLockBase, G, P> Deref for Locked<R, G, P> {
    type Target = P;

    fn deref(&self) -> &P {
        &self.published
    }
}

impl<R: RawReadLock, G: Sync, P> Locked<R, G, P> {
    pub fn read(&self) -> SharedGuard<'_, R, G, P> {
        let state = unsafe { self.raw.lock_read() };
        SharedGuard { lock: self, state: Some(state), _not_send: PhantomData }
    }

    /// read(), if no writer holds it.
    pub fn try_read(&self) -> Option<SharedGuard<'_, R, G, P>>
    where
        R: RawTryReadLock,
    {
        let state = unsafe { self.raw.try_lock_read() }?;
        Some(SharedGuard { lock: self, state: Some(state), _not_send: PhantomData })
    }
}

impl<R: RawRwLock, G: Sync, P> Locked<R, G, P> {
    pub fn write(&self) -> ExclusiveGuard<'_, R, G, P> {
        self.lock()
    }

    pub fn assert_held_any(&self) {
        unsafe { self.raw.assert_held_any() }
    }
}

impl<R: RawSixLock, G: Sync, P> Locked<R, G, P> {
    pub fn intent(&self) -> SharedGuard<'_, R, G, P, SixIntent> {
        let state = unsafe { self.raw.lock_intent() };
        SharedGuard { lock: self, state: Some(state), _not_send: PhantomData }
    }
}

impl<R: RawLockBase, G, P> Drop for Locked<R, G, P> {
    fn drop(&mut self) {
        unsafe { self.raw.destroy() }
    }
}

impl<R: RawLockBase, G, P, A> Placed<Locked<R, G, P>, A> {
    /// As Locked::get_mut(): the lock stays where it is.
    pub fn get_mut(&mut self) -> &mut G {
        unsafe { &mut *self.protected.get() }
    }
}

/// A Locked as a Placed holds it.
pub trait AsLocked {
    type R: RawLockBase;
    type G;
    type P;
    const LOCKED_OFFSET: usize;
    fn locked(&self) -> &Locked<Self::R, Self::G, Self::P>;
}

impl<R: RawLockBase, G, P> AsLocked for Locked<R, G, P> {
    type R = R;
    type G = G;
    type P = P;
    const LOCKED_OFFSET: usize = 0;
    fn locked(&self) -> &Self {
        self
    }
}

/// A lock placed in its parent, at A: its guards carry A, so a guard knows
/// where its lock is.
impl<C: AsLocked, A> Placed<C, A> {
    pub fn lock(&self) -> ExclusiveGuard<'_, C::R, C::G, C::P, Exclusive, A>
    where
        C::R: RawLock,
    {
        self.locked().lock().placed()
    }

    pub fn try_lock(&self) -> Option<ExclusiveGuard<'_, C::R, C::G, C::P, Exclusive, A>>
    where
        C::R: RawTryLock,
    {
        self.locked().try_lock().map(ExclusiveGuard::placed)
    }

    pub fn read(&self) -> SharedGuard<'_, C::R, C::G, C::P, Shared, A>
    where
        C::R: RawReadLock,
        C::G: Sync,
    {
        self.locked().read().placed()
    }

    pub fn write(&self) -> ExclusiveGuard<'_, C::R, C::G, C::P, Exclusive, A>
    where
        C::R: RawRwLock,
        C::G: Sync,
    {
        self.locked().write().placed()
    }
}

/// How a guard's lock is released: which of its kind's unlocks. A guard's
/// type says how it was taken - a six lock's read and intent are both shared,
/// and released differently.
pub trait Unlock<R: RawLockBase> {
    /// # Safety
    /// @raw is held as this unlock releases, taken with @state.
    unsafe fn unlock(raw: &R, state: R::GuardState);
}

/// RawLock's unlock().
pub struct Exclusive;

impl<R: RawLock> Unlock<R> for Exclusive {
    unsafe fn unlock(raw: &R, state: R::GuardState) {
        unsafe { raw.unlock(state) }
    }
}

/// RawReadLock's unlock_read().
pub struct Shared;

impl<R: RawReadLock> Unlock<R> for Shared {
    unsafe fn unlock(raw: &R, state: R::GuardState) {
        unsafe { raw.unlock_read(state) }
    }
}

pub struct SixIntent;

impl<R: RawSixLock> Unlock<R> for SixIntent {
    unsafe fn unlock(raw: &R, state: R::GuardState) {
        unsafe { raw.unlock_intent(state) }
    }
}

/// Back to intent alone.
pub struct SixWrite;

impl<R: RawSixLock> Unlock<R> for SixWrite {
    unsafe fn unlock(raw: &R, state: R::GuardState) {
        unsafe { raw.unlock_write(state) }
    }
}

/// Write and the intent under it, held by one guard: released, both go.
pub struct SixWriteIntent;

impl<R: RawSixLock<GuardState = ()>> Unlock<R> for SixWriteIntent {
    unsafe fn unlock(raw: &R, (): ()) {
        unsafe {
            raw.unlock_write(());
            raw.unlock_intent(());
        }
    }
}

/// A held Locked, had alone: released when dropped, by @U, if it was taken
/// for the guard. Not Send: released by the task that took it, as lockdep
/// checks - and a spin_lock_irqsave()'s interrupt state is its CPU's. @A is
/// the lock's place, a Placed<Locked, A>'s, or () for a lock that isn't
/// placed.
#[must_use = "the lock is released as soon as the guard is dropped"]
pub struct ExclusiveGuard<'a, R: RawLockBase, G = (), P = (), U: Unlock<R> = Exclusive, A = ()> {
    lock:      &'a Locked<R, G, P>,
    /// None for a lock held by its caller - compat.rs's held(): not ours to
    /// release
    state:     Option<R::GuardState>,
    _not_send: PhantomData<(*mut (), U, A)>,
}

impl<'a, R: RawLockBase, G, P, U: Unlock<R>> ExclusiveGuard<'a, R, G, P, U> {
    /// # Safety
    /// @lock is held exclusively, as @U releases: taken with @state, or by our
    /// caller for as long as the guard lives, if None.
    pub(super) unsafe fn from_held(lock: &'a Locked<R, G, P>, state: Option<R::GuardState>) -> Self {
        ExclusiveGuard { lock, state, _not_send: PhantomData }
    }
}

impl<'a, R: RawLockBase, G, P, U: Unlock<R>, A> ExclusiveGuard<'a, R, G, P, U, A> {
    pub(crate) fn lock(&self) -> &'a Locked<R, G, P> {
        self.lock
    }

    /// The guard, of a lock placed at @B: Placed<Locked, B>'s alone to call.
    pub(super) fn placed<B>(self) -> ExclusiveGuard<'a, R, G, P, U, B> {
        let mut g = core::mem::ManuallyDrop::new(self);
        ExclusiveGuard { lock: g.lock, state: g.state.take(), _not_send: PhantomData }
    }

    pub fn protected(&mut self) -> &mut G {
        self
    }

    /// Borrows the guard mutably, so writers had through it are one at a time:
    /// a FakePlaceRef held across another's store is caught, as far as it can
    /// be.
    pub fn published(&mut self) -> &P::Writer
    where
        P: Publish,
    {
        // P::Writer is laid out as P (Publish)
        unsafe { &*core::ptr::from_ref(&self.lock.published).cast::<P::Writer>() }
    }

    /// protected() and published() at once: borrowing the guard once for both.
    pub fn parts(&mut self) -> (&mut G, &P::Writer)
    where
        P: Publish,
    {
        unsafe {
            (&mut *self.lock.protected.get(),
             &*core::ptr::from_ref(&self.lock.published).cast::<P::Writer>())
        }
    }
}

impl<R: RawLockBase, G, P, U: Unlock<R>, A> Deref for ExclusiveGuard<'_, R, G, P, U, A> {
    type Target = G;

    fn deref(&self) -> &G {
        unsafe { &*self.lock.protected.get() }
    }
}

impl<R: RawLockBase, G, P, U: Unlock<R>, A> DerefMut for ExclusiveGuard<'_, R, G, P, U, A> {
    fn deref_mut(&mut self) -> &mut G {
        unsafe { &mut *self.lock.protected.get() }
    }
}

impl<R: RawLockBase, G, P, U: Unlock<R>, A> Drop for ExclusiveGuard<'_, R, G, P, U, A> {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            unsafe { U::unlock(&self.lock.raw, state) }
        }
    }
}

/// A held Locked, had with others: as ExclusiveGuard, but G is only read.
#[must_use = "the lock is released as soon as the guard is dropped"]
pub struct SharedGuard<'a, R: RawLockBase, G = (), P = (), U: Unlock<R> = Shared, A = ()> {
    lock:      &'a Locked<R, G, P>,
    /// Some till drop takes it
    state:     Option<R::GuardState>,
    _not_send: PhantomData<(*mut (), U, A)>,
}

impl<'a, R: RawLockBase, G, P, U: Unlock<R>> SharedGuard<'a, R, G, P, U> {
    /// # Safety
    /// @lock is held shared, as @U releases, taken with @state.
    pub(super) unsafe fn from_held(lock: &'a Locked<R, G, P>, state: R::GuardState) -> Self {
        SharedGuard { lock, state: Some(state), _not_send: PhantomData }
    }
}

impl<'a, R: RawLockBase, G, P, U: Unlock<R>, A> SharedGuard<'a, R, G, P, U, A> {
    pub(super) fn lock(&self) -> &'a Locked<R, G, P> {
        self.lock
    }

    /// As ExclusiveGuard::placed().
    pub(super) fn placed<B>(self) -> SharedGuard<'a, R, G, P, U, B> {
        let mut g = core::mem::ManuallyDrop::new(self);
        SharedGuard { lock: g.lock, state: g.state.take(), _not_send: PhantomData }
    }

    pub fn protected(&self) -> &G {
        self
    }

    pub fn published(&self) -> &'a P {
        &self.lock.published
    }
}

impl<R: RawSixLock, G, P, A> SharedGuard<'_, R, G, P, SixIntent, A> {
    /// Write, with intent held: G exclusively, once readers drain. Borrows
    /// the guard mutably, so nothing had through it as intent is held across;
    /// dropped, back to intent alone.
    pub fn write(&mut self) -> ExclusiveGuard<'_, R, G, P, SixWrite, A> {
        let state = unsafe { self.lock.raw.lock_write() };
        ExclusiveGuard { lock: self.lock, state: Some(state), _not_send: PhantomData }
    }

    /// P as its writer has it: the intent holder is the only one, with
    /// readers still about.
    pub fn published_writer(&mut self) -> &P::Writer
    where
        P: Publish,
    {
        // P::Writer is laid out as P (Publish)
        unsafe { &*core::ptr::from_ref(&self.lock.published).cast::<P::Writer>() }
    }
}

impl<R: RawLockBase, G, P, U: Unlock<R>, A> Deref for SharedGuard<'_, R, G, P, U, A> {
    type Target = G;

    fn deref(&self) -> &G {
        unsafe { &*self.lock.protected.get() }
    }
}

impl<R: RawLockBase, G, P, U: Unlock<R>, A> Drop for SharedGuard<'_, R, G, P, U, A> {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            unsafe { U::unlock(&self.lock.raw, state) }
        }
    }
}

/// What a LockedBy names for its L, and is had through: a placed lock's
/// guard, by the lock's place. Its root is where paths to what it covers
/// start: the struct its lock is placed in. So writer::<field!(opts)>() of an
/// sb_lock guard is its own filesystem's opts - nothing to check.
///
/// get_of() and the rest are had by reference instead, for a LockedBy no path
/// reaches - a device's, under its filesystem's lock: checked to name this
/// guard's place, but not that it's this lock - not another filesystem's.
///
/// # Safety
/// While one lives, what Name names is held - shared, or alone if Alone - and
/// root() is the struct it's in.
pub(crate) unsafe trait Guard {
    type Name;
    type Root;

    fn root(&self) -> &Self::Root;

    fn writer<'g, F>(&'g self) -> &'g <<F::To as AsLockedBy>::P as Publish>::Writer
    where
        F: Path<Self::Root>,
        F::To: AsLockedBy<L = Self::Name, G: 'g, P: Publish + 'g> + 'g,
    {
        unsafe { F::get(self.root()).locked_by().published_writer() }
    }

    fn get_of<'g, C: AsLockedBy<L = Self::Name>>(&'g self, c: &'g C) -> &'g C::G {
        unsafe { c.locked_by().protected() }
    }

    fn get_mut_of<'g, C: AsLockedBy<L = Self::Name>>(&'g mut self, c: &'g C) -> &'g mut C::G
    where
        Self: Alone,
    {
        unsafe { c.locked_by().protected_mut() }
    }

    fn writer_of<'g, C>(&'g self, c: &'g C) -> &'g <C::P as Publish>::Writer
    where
        C: AsLockedBy<L = Self::Name, P: Publish>,
    {
        unsafe { c.locked_by().published_writer() }
    }

    /// Several of what it covers at once, by path from the root: @S a tuple of
    /// Mut<field!(..)> and Ref<field!(..)>, whose bytes are checked when it's
    /// compiled not to overlap - but two Refs'.
    fn split<'g, S: Split<'g, Self>>(&'g mut self) -> S::Out
    where
        Self: Sized + 'g,
    {
        let () = S::DISJOINT;
        unsafe { S::get(self) }
    }
}

/// A Guard held alone: what it covers is had by &mut.
///
/// # Safety
/// While one lives, nothing else holds what it names.
pub(crate) unsafe trait Alone: Guard {}

// ── Splits: several pieces under one guard ──────────────────────────────────

/// A Guard holding a ref on the AtomicRef placed at @A: its value is live
/// while the guard is.
///
/// # Safety
/// While one lives, a ref on the AtomicRef at A is held.
pub(crate) unsafe trait HoldsRef<A>: Guard {}

/// A path from a guard's root down to what a split of it hands out - as
/// field!() writes one, of fields and the values of refs the guard holds: at
/// OFFSET of the root, as many bytes as a To.
///
/// # Safety
/// get() follows the path, to the To at OFFSET of the guard's root.
pub(crate) unsafe trait GuardPath<G: Guard> {
    type To;
    const OFFSET: usize;
    fn get(g: &G) -> &Self::To;
}

unsafe impl<G: Guard> GuardPath<G> for Here {
    type To = G::Root;
    const OFFSET: usize = 0;
    fn get(g: &G) -> &G::Root {
        g.root()
    }
}

unsafe impl<G: Guard, P: GuardPath<G>, const N: u64> GuardPath<G> for FieldOf<P, N>
where
    P::To: Field<N>,
{
    type To = <P::To as Field<N>>::Type;
    const OFFSET: usize = P::OFFSET + <P::To as Field<N>>::OFFSET;
    fn get(g: &G) -> &Self::To {
        unsafe { &*core::ptr::from_ref(P::get(g)).byte_add(<P::To as Field<N>>::OFFSET).cast() }
    }
}

unsafe impl<G, P, T, A: 'static> GuardPath<G> for RefValue<P>
where
    G: HoldsRef<A>,
    P: GuardPath<G, To = Placed<AtomicRef<T>, A>>,
{
    type To = T;
    const OFFSET: usize = P::OFFSET + Placed::<AtomicRef<T>, A>::VALUE_OFFSET + AtomicRef::<T>::VALUE_OFFSET;
    fn get(g: &G) -> &T {
        unsafe { P::get(g).value_held() }
    }
}

/// A piece of a split: Mut<P>, what the LockedBy at P protects, by &mut - of
/// a guard held alone - and Ref<P>, by &.
pub(crate) struct Mut<P>(PhantomData<P>);
pub(crate) struct Ref<P>(PhantomData<P>);

/// # Safety
/// get() hands out what's in [OFFSET, OFFSET + SIZE) of the guard's root, and
/// only that: by &mut if MUT.
pub(crate) unsafe trait Piece<'g, G: Guard + 'g> {
    type Out;
    const OFFSET: usize;
    const SIZE: usize;
    const MUT: bool;

    /// # Safety
    /// Nothing else refers to what it hands out while that lives - or, but
    /// for MUT, nothing by &mut.
    unsafe fn get(g: &'g G) -> Self::Out;
}

unsafe impl<'g, G: Alone + 'g, P: GuardPath<G>> Piece<'g, G> for Mut<P>
where
    P::To: AsLockedBy<L = G::Name> + 'g,
    <P::To as AsLockedBy>::G: 'g,
{
    type Out = &'g mut <P::To as AsLockedBy>::G;
    const OFFSET: usize = P::OFFSET;
    const SIZE: usize = size_of::<P::To>();
    const MUT: bool = true;

    unsafe fn get(g: &'g G) -> Self::Out {
        unsafe { P::get(g).locked_by().protected_mut() }
    }
}

unsafe impl<'g, G: Guard + 'g, P: GuardPath<G>> Piece<'g, G> for Ref<P>
where
    P::To: AsLockedBy<L = G::Name> + 'g,
    <P::To as AsLockedBy>::G: 'g,
{
    type Out = &'g <P::To as AsLockedBy>::G;
    const OFFSET: usize = P::OFFSET;
    const SIZE: usize = size_of::<P::To>();
    const MUT: bool = false;

    unsafe fn get(g: &'g G) -> Self::Out {
        unsafe { P::get(g).locked_by().protected() }
    }
}

/// A tuple of Pieces, Guard::split()'s.
///
/// # Safety
/// DISJOINT evaluates only if the pieces' bytes don't overlap, but two
/// shared pieces'; get() hands out each piece.
pub(crate) unsafe trait Split<'g, G: Guard + 'g> {
    type Out;
    const DISJOINT: ();

    /// # Safety
    /// As Piece::get()'s, for every piece.
    unsafe fn get(g: &'g G) -> Self::Out;
}

/// Whether pieces (offset, size, mut) overlap only where both are shared.
const fn pieces_disjoint(p: &[(usize, usize, bool)]) -> bool {
    let mut i = 0;
    while i < p.len() {
        let mut j = i + 1;
        while j < p.len() {
            let ((a, a_len, a_mut), (b, b_len, b_mut)) = (p[i], p[j]);
            if (a_mut || b_mut) && a < b + b_len && b < a + a_len {
                return false;
            }
            j += 1;
        }
        i += 1;
    }
    true
}

macro_rules! split_tuple {
    ($($p:ident),+) => {
        unsafe impl<'g, G: Guard + 'g, $($p: Piece<'g, G>),+> Split<'g, G> for ($($p,)+) {
            type Out = ($($p::Out,)+);
            const DISJOINT: () = assert!(pieces_disjoint(&[$(($p::OFFSET, $p::SIZE, $p::MUT)),+]),
                                         "a split's pieces overlap");

            unsafe fn get(g: &'g G) -> Self::Out {
                unsafe { ($($p::get(g),)+) }
            }
        }
    };
}
split_tuple!(A, B);
split_tuple!(A, B, C);

unsafe impl<'a, R: RawLockBase, G, P, U: Unlock<R>, A: Place + 'a> Guard for SharedGuard<'a, R, G, P, U, A>
where
    A::Type: Contained + PlacedLock<R, G, P>,
{
    type Name = A;
    type Root = <A::Type as Contained>::Parent;

    fn root(&self) -> &Self::Root {
        unsafe { A::Type::of_lock(self.lock) }.container()
    }
}

unsafe impl<'a, R: RawLockBase, G, P, U: Unlock<R>, A: Place + 'a> Guard for ExclusiveGuard<'a, R, G, P, U, A>
where
    A::Type: Contained + PlacedLock<R, G, P>,
{
    type Name = A;
    type Root = <A::Type as Contained>::Parent;

    fn root(&self) -> &Self::Root {
        unsafe { A::Type::of_lock(self.lock) }.container()
    }
}

/// A placed lock's field: what its guards' root() finds it by.
pub(crate) trait PlacedLock<R: RawLockBase, G, P> {
    /// The field @lock is in.
    ///
    /// # Safety
    /// @lock is a guard of this field's place's: Placed's lock methods alone
    /// make one, of the Locked it holds.
    unsafe fn of_lock(lock: &Locked<R, G, P>) -> &Self;
}

impl<C: AsLocked, A> PlacedLock<C::R, C::G, C::P> for Placed<C, A> {
    unsafe fn of_lock(lock: &Locked<C::R, C::G, C::P>) -> &Self {
        // the value's at a Placed's start
        unsafe { &*core::ptr::from_ref(lock).byte_sub(C::LOCKED_OFFSET).cast() }
    }
}

// an exclusive guard's U is a mode no other holder shares
unsafe impl<'a, R: RawLockBase, G, P, U: Unlock<R>, A: Place + 'a> Alone for ExclusiveGuard<'a, R, G, P, U, A>
where
    A::Type: Contained + PlacedLock<R, G, P>,
{}

/// Locked's two halves, for data that can't live in its lock: @G, reached only
/// with that lock held, and @P, published - anyone's, it derefs to it. Laid
/// out as G then P, as Locked's are.
///
/// Which lock that is, @L names: its place - at!(Fs.sb), of a Placed<Locked>
/// - whose guards have it, as a Guard. With none, () - only the owner knows,
/// and has it only unsafely.
#[repr(C)]
pub struct LockedBy<G, P = (), L = ()> {
    protected: UnsafeCell<G>,
    published: P,
    _lock:     PhantomData<fn() -> L>,
}

// As Locked's.
unsafe impl<G: Send, P: Send, L> Send for LockedBy<G, P, L> {}
unsafe impl<G: Send, P: Sync, L> Sync for LockedBy<G, P, L> {}

// SAFETY: as each half
unsafe impl<G: ::pin_init::Zeroable, P: ::pin_init::Zeroable, L> ::pin_init::Zeroable for LockedBy<G, P, L> {}

impl<G, P, L> LockedBy<G, P, L> {
    /// Where the G is in a LockedBy.
    pub const PROTECTED_OFFSET: usize = core::mem::offset_of!(Self, protected);

    pub const fn new(protected: G, published: P) -> Self {
        LockedBy { protected: UnsafeCell::new(protected), published, _lock: PhantomData }
    }

    /// What it protects, unlocked: for a racy peek, as Locked::data_ptr().
    pub fn data_ptr(&self) -> *const G {
        self.protected.get()
    }

    /// # Safety
    /// The lock that protects it is held, for as long as the reference
    /// lives, and nothing has a &mut of it.
    pub unsafe fn protected(&self) -> &G {
        unsafe { &*self.protected.get() }
    }

    /// # Safety
    /// The lock that protects it is held, for as long as the reference
    /// lives, and nothing else refers to it.
    pub unsafe fn protected_mut(&self) -> &mut G {
        unsafe { &mut *self.protected.get() }
    }

    /// G, had through the only reference to its owner.
    pub fn get_mut(&mut self) -> &mut G {
        self.protected.get_mut()
    }
}

impl<G, P: crate::util::projections::Publish, L> LockedBy<G, P, L> {
    /// @P as its writer has it: the owner's to hand out, to whoever shows it
    /// holds the lock.
    ///
    /// # Safety
    /// The lock that publishes it is held, for as long as the reference lives.
    pub unsafe fn published_writer(&self) -> &P::Writer {
        // SAFETY: Publish's - a &P is a valid &P::Writer
        unsafe { &*core::ptr::from_ref(&self.published).cast::<P::Writer>() }
    }

    /// @P as its writer has it, through the only reference to it.
    pub fn published_writer_mut(&mut self) -> &P::Writer {
        unsafe { self.published_writer() }
    }
}

impl<G, P, L> Deref for LockedBy<G, P, L> {
    type Target = P;

    fn deref(&self) -> &P {
        &self.published
    }
}

/// A LockedBy as a struct holds it.
pub trait AsLockedBy {
    type G;
    type P;
    type L;
    fn locked_by(&self) -> &LockedBy<Self::G, Self::P, Self::L>;
}

impl<G, P, L> AsLockedBy for LockedBy<G, P, L> {
    type G = G;
    type P = P;
    type L = L;
    fn locked_by(&self) -> &Self {
        self
    }
}

// Opaque to TypeInfo, size only: G is the owner's to read.
impl<G, P, L> crate::typeinfo::TypeInfo for LockedBy<G, P, L> {
    const INFO: &'static crate::typeinfo::StructInfo = &crate::typeinfo::StructInfo {
        name:   "LockedBy",
        size:   core::mem::size_of::<Self>(),
        shape:  crate::typeinfo::Shape::Struct,
        fields: &[],
    };
}

/// A Locked initializer - new_lock!(g), new_lock!(g, p), either with a "name"
/// last - over @g, publishing @p or nothing, with a lockdep class of its own,
/// named @name or for the call site. new_lock!(<- init), and
/// new_lock!(<- init, p, "name"), initialize @g in place, by @init:
/// Locked::new_init().
#[macro_export]
macro_rules! new_lock {
    (@key) => {{
        #[cfg(kernel)]
        let key = ::kernel::static_lock_class!().as_ptr().cast();
        #[cfg(not(kernel))]
        let key = ::core::ptr::null_mut();
        key
    }};
    (@init $g:expr, $p:expr, $name:expr) => {{
        let key = $crate::new_lock!(@key);
        let g = $g;
        let p = $p;
        let name = const { $crate::util::ffi::c_str($name) };
        unsafe { $crate::locking::Locked::new(g, p, name, key) }
    }};
    (@init_in_place $g:expr, $p:expr, $name:expr) => {{
        let key = $crate::new_lock!(@key);
        let g = $g;
        let p = $p;
        let name = const { $crate::util::ffi::c_str($name) };
        unsafe { $crate::locking::Locked::new_init(g, p, name, key) }
    }};
    (<- $g:expr $(,)?) => {
        $crate::new_lock!(@init_in_place $g, (), ::core::concat!(::core::file!(), ":", ::core::line!(), "\0"))
    };
    (<- $g:expr, $p:expr, $name:literal $(,)?) => {
        $crate::new_lock!(@init_in_place $g, $p, ::core::concat!($name, "\0"))
    };
    ($g:expr $(,)?) => {
        $crate::new_lock!(@init $g, (), ::core::concat!(::core::file!(), ":", ::core::line!(), "\0"))
    };
    ($g:expr, $name:literal $(,)?) => {
        $crate::new_lock!(@init $g, (), ::core::concat!($name, "\0"))
    };
    ($g:expr, $p:expr $(,)?) => {
        $crate::new_lock!(@init $g, $p, ::core::concat!(::core::file!(), ":", ::core::line!(), "\0"))
    };
    ($g:expr, $p:expr, $name:literal $(,)?) => {
        $crate::new_lock!(@init $g, $p, ::core::concat!($name, "\0"))
    };
}
