// SPDX-License-Identifier: GPL-2.0

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

/// A RawLock that readers can share: RawLock's lock() and unlock() are the
/// writer's, alone.
///
/// # Safety
/// lock_shared() excludes the writer, and only the writer, until
/// unlock_shared(); as RawLock's otherwise.
pub unsafe trait RawRwLock: RawLock {
    unsafe fn lock_shared(&self) -> Self::GuardState;
    unsafe fn unlock_shared(&self, g: Self::GuardState);
    /// lockdep_assert_held(): by a reader or the writer.
    unsafe fn assert_held_any(&self);
}

/// Two things locked by @R: @G, protected - reached only with it held, its
/// guard derefs to it - and @P, published: anyone's, the lock derefs to it,
/// held or not; written through the guard's published(), so the lock a
/// writer holds is the one it's in. Laid out as C's members, the lock then G
/// then P, so a struct C shares can declare it as them:
/// #[c_lock("struct mutex lock")].
#[repr(C)]
pub struct Locked<R: RawLockBase, G = (), P = ()> {
    raw:       R,
    protected: UnsafeCell<G>,
    published: P,
    // pinned - a kernel lock mustn't move - in a PhantomData: a bare
    // PhantomPinned isn't FFI-safe in a repr(C) struct, and C's hold these
    _pin:      PhantomData<PhantomPinned>,
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

    /// Where the P is: for #[c_lock]'s asserts.
    pub const PUBLISHED_OFFSET: usize = core::mem::offset_of!(Self, published);

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
}

impl<R: RawLockBase, G, P> Deref for Locked<R, G, P> {
    type Target = P;

    fn deref(&self) -> &P {
        &self.published
    }
}

impl<R: RawRwLock, G: Sync, P> Locked<R, G, P> {
    pub fn read(&self) -> SharedGuard<'_, R, G, P> {
        let state = unsafe { self.raw.lock_shared() };
        SharedGuard { lock: self, state: Some(state), _not_send: PhantomData }
    }

    pub fn write(&self) -> ExclusiveGuard<'_, R, G, P> {
        self.lock()
    }

    pub fn assert_held_any(&self) {
        unsafe { self.raw.assert_held_any() }
    }
}

impl<R: RawLockBase, G, P> Drop for Locked<R, G, P> {
    fn drop(&mut self) {
        unsafe { self.raw.destroy() }
    }
}

/// A held Locked: released when dropped, if lock() took it. Not Send: released
/// by the task that took it, as lockdep checks - and a spin_lock_irqsave()'s
/// interrupt state is its CPU's.
#[must_use = "the lock is released as soon as the guard is dropped"]
pub struct ExclusiveGuard<'a, R: RawLock, G = (), P = ()> {
    lock:      &'a Locked<R, G, P>,
    /// None for a lock held by its caller - compat.rs's held(): not ours to
    /// release
    state:     Option<R::GuardState>,
    _not_send: PhantomData<*mut ()>,
}

impl<'a, R: RawLock, G, P> ExclusiveGuard<'a, R, G, P> {
    /// # Safety
    /// @lock is held exclusively: taken with @state, or by our caller for as
    /// long as the guard lives, if None.
    pub(super) unsafe fn from_held(lock: &'a Locked<R, G, P>, state: Option<R::GuardState>) -> Self {
        ExclusiveGuard { lock, state, _not_send: PhantomData }
    }

    pub(crate) fn lock(&self) -> &'a Locked<R, G, P> {
        self.lock
    }
}

impl<R: RawLock, G, P> ExclusiveGuard<'_, R, G, P> {
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

impl<R: RawLock, G, P> Deref for ExclusiveGuard<'_, R, G, P> {
    type Target = G;

    fn deref(&self) -> &G {
        unsafe { &*self.lock.protected.get() }
    }
}

impl<R: RawLock, G, P> DerefMut for ExclusiveGuard<'_, R, G, P> {
    fn deref_mut(&mut self) -> &mut G {
        unsafe { &mut *self.lock.protected.get() }
    }
}

impl<R: RawLock, G, P> Drop for ExclusiveGuard<'_, R, G, P> {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            unsafe { self.lock.raw.unlock(state) }
        }
    }
}

#[must_use = "the lock is released as soon as the guard is dropped"]
pub struct SharedGuard<'a, R: RawRwLock, G = (), P = ()> {
    lock:      &'a Locked<R, G, P>,
    // an Option only to be taken in drop()
    state:     Option<R::GuardState>,
    _not_send: PhantomData<*mut ()>,
}

impl<'a, R: RawRwLock, G, P> SharedGuard<'a, R, G, P> {
    pub(super) unsafe fn from_held(lock: &'a Locked<R, G, P>, state: Option<R::GuardState>) -> Self {
        SharedGuard { lock, state, _not_send: PhantomData }
    }

    pub(super) fn lock(&self) -> &'a Locked<R, G, P> {
        self.lock
    }

    pub fn protected(&self) -> &G {
        self
    }

    pub fn published(&self) -> &'a P {
        &self.lock.published
    }
}

impl<R: RawRwLock, G, P> Deref for SharedGuard<'_, R, G, P> {
    type Target = G;

    fn deref(&self) -> &G {
        unsafe { &*self.lock.protected.get() }
    }
}

impl<R: RawRwLock, G, P> Drop for SharedGuard<'_, R, G, P> {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            unsafe { self.lock.raw.unlock_shared(state) }
        }
    }
}

/// Locked's two halves, for data that can't live in its lock: @G, reached only
/// with that lock held, and @P, published - anyone's, it derefs to it. Which
/// lock that is, only the owner of both knows: G is reached through the
/// owner's methods, which check - here, only unsafely. Laid out as G then P,
/// as Locked's are.
#[repr(C)]
pub struct LockedBy<G, P = ()> {
    protected: UnsafeCell<G>,
    published: P,
}

// As Locked's.
unsafe impl<G: Send, P: Send> Send for LockedBy<G, P> {}
unsafe impl<G: Send, P: Sync> Sync for LockedBy<G, P> {}

// SAFETY: as each half
unsafe impl<G: ::pin_init::Zeroable, P: ::pin_init::Zeroable> ::pin_init::Zeroable for LockedBy<G, P> {}

impl<G, P> LockedBy<G, P> {
    pub const fn new(protected: G, published: P) -> Self {
        LockedBy { protected: UnsafeCell::new(protected), published }
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
}

impl<G, P: crate::util::projections::Publish> LockedBy<G, P> {
    /// @P as its writer has it: the owner's to hand out, to whoever shows it
    /// holds the lock.
    ///
    /// # Safety
    /// The lock that publishes it is held, for as long as the reference lives.
    pub unsafe fn published_writer(&self) -> &P::Writer {
        // SAFETY: Publish's - a &P is a valid &P::Writer
        unsafe { &*core::ptr::from_ref(&self.published).cast::<P::Writer>() }
    }
}

impl<G, P> Deref for LockedBy<G, P> {
    type Target = P;

    fn deref(&self) -> &P {
        &self.published
    }
}

// Opaque to TypeInfo, size only: G is the owner's to read.
impl<G, P> crate::typeinfo::TypeInfo for LockedBy<G, P> {
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
