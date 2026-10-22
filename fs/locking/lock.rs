// SPDX-License-Identifier: GPL-2.0

use crate::util::projections::Publish;
use core::cell::UnsafeCell;
use core::ffi::{c_void, CStr};
use core::marker::{PhantomData, PhantomPinned};
use core::ops::{Deref, DerefMut};

/// C's lock state, and how it's initialized, taken and released -
/// implemented on C's lock types themselves, where the kernel crate's lock
/// Backend is a marker over them.
///
/// # Safety
/// lock() excludes every other holder until unlock(). All of its state is
/// interior-mutable - C's locks are Opaques, ours atomics - so it's taken and
/// released through a shared reference, from any thread. Callers pass an
/// initialized lock, except to init() - for a C lock, a & to one zeroed
/// isn't one: a zeroed kernel mutex's wait list is null - and unlock() the
/// state their lock() returned.
pub unsafe trait RawLock {
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

    unsafe fn lock(&self) -> Self::GuardState;

    unsafe fn unlock(&self, g: Self::GuardState);

    /// That it's held exclusively - lockdep_assert_held(), and
    /// lockdep_assert_held_write() for a lock readers can share: a reader
    /// holding it isn't enough.
    unsafe fn assert_held(&self);

    /// For a lock that allocates; nothing holds it or will.
    unsafe fn destroy(&self) {}
}

/// A RawLock that can be tried: taken only if no one holds it.
///
/// # Safety
/// try_lock() is RawLock's lock(), when it returns Some; when None, nothing's
/// taken, and nothing's left to release.
pub unsafe trait RawTryLock: RawLock {
    unsafe fn try_lock(&self) -> Option<Self::GuardState>;
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

/// Lock @R, over two things: @G, protected - reached only with it held, its
/// guard derefs to it - and @P, published: anyone's, the lock derefs to it,
/// held or not; written through the guard's published(), so the lock a
/// writer holds is the one it's in. Laid out as C's members, the lock then G
/// then P, so a struct C shares can declare it as them:
/// #[c_anon("struct mutex lock; G g")].
#[repr(C)]
pub struct Lock<R: RawLock, G = (), P = ()> {
    raw:       R,
    protected: UnsafeCell<G>,
    published: P,
    // pinned - a kernel lock mustn't move - in a PhantomData: a bare
    // PhantomPinned isn't FFI-safe in a repr(C) struct, and C's hold these
    _pin:      PhantomData<PhantomPinned>,
}

// G is had by one thread at a time, as a std Mutex's; P is every thread's.
// read() shares G, so it wants G: Sync.
unsafe impl<R: RawLock, G: Send, P: Send> Send for Lock<R, G, P> {}
unsafe impl<R: RawLock, G: Send, P: Sync> Sync for Lock<R, G, P> {}

impl<R: RawLock, G, P> Lock<R, G, P> {
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

    /// For a Lock in zeroed memory that new() can't reach - a static: its
    /// raw lock initialized in place, G and P as zeroed.
    ///
    /// # Safety
    /// Once, before anything takes it; zeroed is a valid G and P. @key is as
    /// new()'s.
    pub unsafe fn init_zeroed(&self, name: &'static CStr, key: *mut c_void) -> Result<(), R::InitError> {
        unsafe { R::init(core::ptr::from_ref(&self.raw).cast_mut(), name, key) }
    }

    pub fn lock(&self) -> Guard<'_, R, G, P> {
        let state = unsafe { self.raw.lock() };
        Guard { lock: self, state: Some(state), _not_send: PhantomData }
    }

    /// lock(), if no one holds it.
    pub fn try_lock(&self) -> Option<Guard<'_, R, G, P>>
    where
        R: RawTryLock,
    {
        let state = unsafe { self.raw.try_lock() }?;
        Some(Guard { lock: self, state: Some(state), _not_send: PhantomData })
    }

    /// Held exclusively - by the writer, for a lock readers share.
    pub fn assert_held(&self) {
        unsafe { self.raw.assert_held() }
    }

    /// Where the G is in a Lock: for C's container_of() from it.
    pub const PROTECTED_OFFSET: usize = core::mem::offset_of!(Self, protected);

    pub(super) fn raw(&self) -> &R {
        &self.raw
    }
}

impl<R: RawLock, G, P> Deref for Lock<R, G, P> {
    type Target = P;

    fn deref(&self) -> &P {
        &self.published
    }
}

impl<R: RawRwLock, G: Sync, P> Lock<R, G, P> {
    pub fn read(&self) -> ReadGuard<'_, R, G, P> {
        let state = unsafe { self.raw.lock_shared() };
        ReadGuard { lock: self, state: Some(state), _not_send: PhantomData }
    }

    pub fn write(&self) -> Guard<'_, R, G, P> {
        self.lock()
    }

    pub fn assert_held_any(&self) {
        unsafe { self.raw.assert_held_any() }
    }
}

impl<R: RawLock, G, P> Drop for Lock<R, G, P> {
    fn drop(&mut self) {
        unsafe { self.raw.destroy() }
    }
}

/// A held Lock: released when dropped, if lock() took it. Not Send: released
/// by the task that took it, as lockdep checks - and a spin_lock_irqsave()'s
/// interrupt state is its CPU's.
#[must_use = "the lock is released as soon as the guard is dropped"]
pub struct Guard<'a, R: RawLock, G = (), P = ()> {
    lock:      &'a Lock<R, G, P>,
    /// None for a lock held by its caller - compat.rs's held(): not ours to
    /// release
    state:     Option<R::GuardState>,
    _not_send: PhantomData<*mut ()>,
}

impl<'a, R: RawLock, G, P> Guard<'a, R, G, P> {
    /// # Safety
    /// @lock is held exclusively: taken with @state, or by our caller for as
    /// long as the guard lives, if None.
    pub(super) unsafe fn from_held(lock: &'a Lock<R, G, P>, state: Option<R::GuardState>) -> Self {
        Guard { lock, state, _not_send: PhantomData }
    }

    pub(crate) fn lock(&self) -> &'a Lock<R, G, P> {
        self.lock
    }
}

impl<R: RawLock, G, P> Guard<'_, R, G, P> {
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

impl<R: RawLock, G, P> Deref for Guard<'_, R, G, P> {
    type Target = G;

    fn deref(&self) -> &G {
        unsafe { &*self.lock.protected.get() }
    }
}

impl<R: RawLock, G, P> DerefMut for Guard<'_, R, G, P> {
    fn deref_mut(&mut self) -> &mut G {
        unsafe { &mut *self.lock.protected.get() }
    }
}

impl<R: RawLock, G, P> Drop for Guard<'_, R, G, P> {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            unsafe { self.lock.raw.unlock(state) }
        }
    }
}

#[must_use = "the lock is released as soon as the guard is dropped"]
pub struct ReadGuard<'a, R: RawRwLock, G = (), P = ()> {
    lock:      &'a Lock<R, G, P>,
    // an Option only to be taken in drop()
    state:     Option<R::GuardState>,
    _not_send: PhantomData<*mut ()>,
}

impl<'a, R: RawRwLock, G, P> ReadGuard<'a, R, G, P> {
    pub fn protected(&self) -> &G {
        self
    }

    pub fn published(&self) -> &'a P {
        &self.lock.published
    }
}

impl<R: RawRwLock, G, P> Deref for ReadGuard<'_, R, G, P> {
    type Target = G;

    fn deref(&self) -> &G {
        unsafe { &*self.lock.protected.get() }
    }
}

impl<R: RawRwLock, G, P> Drop for ReadGuard<'_, R, G, P> {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            unsafe { self.lock.raw.unlock_shared(state) }
        }
    }
}

/// Lock's two halves, for data that can't live in its lock: @G, reached only
/// with that lock held, and @P, published - anyone's, it derefs to it. Which
/// lock that is, only the owner of both knows: G is reached through the
/// owner's methods, which check - here, only unsafely. Laid out as G then P,
/// as Lock's are.
#[repr(C)]
pub struct LockedBy<G, P = ()> {
    protected: UnsafeCell<G>,
    published: P,
}

// As Lock's.
unsafe impl<G: Send, P: Send> Send for LockedBy<G, P> {}
unsafe impl<G: Send, P: Sync> Sync for LockedBy<G, P> {}

// SAFETY: as each half
unsafe impl<G: ::pin_init::Zeroable, P: ::pin_init::Zeroable> ::pin_init::Zeroable for LockedBy<G, P> {}

impl<G, P> LockedBy<G, P> {
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

/// A Lock initializer - new_lock!(g), new_lock!(g, p), either with a "name"
/// last - over @g, publishing @p or nothing, with a lockdep class of its own,
/// named @name or for the call site.
#[macro_export]
macro_rules! new_lock {
    (@init $g:expr, $p:expr, $name:expr) => {{
        #[cfg(kernel)]
        let key = ::kernel::static_lock_class!().as_ptr().cast();
        #[cfg(not(kernel))]
        let key = ::core::ptr::null_mut();

        let g = $g;
        let p = $p;
        let name = unsafe { ::core::ffi::CStr::from_bytes_with_nul_unchecked($name.as_bytes()) };
        unsafe { $crate::locking::Lock::new(g, p, name, key) }
    }};
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
