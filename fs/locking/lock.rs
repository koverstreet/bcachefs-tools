// SPDX-License-Identifier: GPL-2.0

use core::cell::UnsafeCell;
use core::ffi::{c_void, CStr};
use core::marker::{PhantomData, PhantomPinned};
use core::ops::{Deref, DerefMut};

/// C's lock state, and how it's initialized, taken and released -
/// implemented on C's lock types themselves, where the kernel crate's lock
/// Backend is a marker over them.
///
/// # Safety
/// lock() excludes every other holder until unlock(); a lock is taken and
/// released through a *mut, from any thread. Callers pass an initialized
/// lock, except to init(), and unlock() the state their lock() returned.
pub unsafe trait RawLock {
    /// What releasing it needs: the memalloc flags a Noio saved, or
    /// nothing.
    type GuardState;
    /// init()'s, for a lock that allocates: Infallible otherwise.
    type InitError;

    /// In lockdep class @key, named @name; @l is pinned from here on.
    unsafe fn init(l: *mut Self, name: &'static CStr, key: *mut c_void)
        -> Result<(), Self::InitError>;

    unsafe fn lock(l: *mut Self) -> Self::GuardState;

    unsafe fn unlock(l: *mut Self, g: Self::GuardState);

    /// That it's held exclusively - lockdep_assert_held(), and
    /// lockdep_assert_held_write() for a lock readers can share: a reader
    /// holding it isn't enough.
    unsafe fn assert_held(l: *mut Self);

    /// For a lock that allocates; nothing holds it or will.
    unsafe fn destroy(_l: *mut Self) {}
}

/// A RawLock that readers can share: RawLock's lock() and unlock() are the
/// writer's, alone.
///
/// # Safety
/// lock_shared() excludes the writer, and only the writer, until
/// unlock_shared(); as RawLock's otherwise.
pub unsafe trait RawRwLock: RawLock {
    unsafe fn lock_shared(l: *mut Self) -> Self::GuardState;
    unsafe fn unlock_shared(l: *mut Self, g: Self::GuardState);
    /// lockdep_assert_held(): by a reader or the writer.
    unsafe fn assert_held_any(l: *mut Self);
}

/// Lock @R, over @G - protected: reached only with it held, its guard derefs
/// to it. Laid out as C's members, the lock then G, so a struct C shares can
/// declare it as them: #[c_anon("struct mutex lock; G g")].
#[repr(C)]
pub struct Lock<R: RawLock, G = ()> {
    raw:       UnsafeCell<R>,
    protected: UnsafeCell<G>,
    // pinned - a kernel lock mustn't move - in a PhantomData: a bare
    // PhantomPinned isn't FFI-safe in a repr(C) struct, and C's hold these
    _pin:      PhantomData<PhantomPinned>,
}

// G is had by one thread at a time, as a std Mutex's. read() shares G, so it
// wants G: Sync.
unsafe impl<R: RawLock, G: Send> Send for Lock<R, G> {}
unsafe impl<R: RawLock, G: Send> Sync for Lock<R, G> {}

impl<R: RawLock, G> Lock<R, G> {
    /// new_lock!()'s, which has a static lockdep key per call site.
    ///
    /// # Safety
    /// @key is a lock class key that lives forever - or null, in userspace.
    #[doc(hidden)]
    pub unsafe fn new(g: G, name: &'static CStr, key: *mut c_void)
        -> impl ::pin_init::PinInit<Self, R::InitError>
    {
        // If init() fails, nothing's written to drop.
        unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut Self| {
                R::init(UnsafeCell::raw_get(&raw const (*slot).raw), name, key)?;
                (&raw mut (*slot).protected).write(UnsafeCell::new(g));
                Ok(())
            })
        }
    }

    pub fn lock(&self) -> Guard<'_, R, G> {
        let state = unsafe { R::lock(self.raw.get()) };
        Guard { lock: self, state: Some(state), _not_send: PhantomData }
    }

    /// Held exclusively - by the writer, for a lock readers share.
    pub fn assert_held(&self) {
        unsafe { R::assert_held(self.raw.get()) }
    }

    pub(super) fn raw(&self) -> *mut R {
        self.raw.get()
    }
}

impl<R: RawRwLock, G: Sync> Lock<R, G> {
    pub fn read(&self) -> ReadGuard<'_, R, G> {
        let state = unsafe { R::lock_shared(self.raw.get()) };
        ReadGuard { lock: self, state: Some(state), _not_send: PhantomData }
    }

    pub fn write(&self) -> Guard<'_, R, G> {
        self.lock()
    }

    pub fn assert_held_any(&self) {
        unsafe { R::assert_held_any(self.raw.get()) }
    }
}

impl<R: RawLock, G> Drop for Lock<R, G> {
    fn drop(&mut self) {
        unsafe { R::destroy(self.raw.get()) }
    }
}

/// A held Lock: released when dropped, if lock() took it. Not Send: released
/// by the task that took it, as lockdep checks.
#[must_use = "the lock is released as soon as the guard is dropped"]
pub struct Guard<'a, R: RawLock, G = ()> {
    lock:      &'a Lock<R, G>,
    /// None for a lock held by its caller - compat.rs's held(): not ours to
    /// release
    state:     Option<R::GuardState>,
    _not_send: PhantomData<*mut ()>,
}

impl<'a, R: RawLock, G> Guard<'a, R, G> {
    /// # Safety
    /// @lock is held exclusively: taken with @state, or by our caller for as
    /// long as the guard lives, if None.
    pub(super) unsafe fn from_held(lock: &'a Lock<R, G>, state: Option<R::GuardState>) -> Self {
        Guard { lock, state, _not_send: PhantomData }
    }

    pub(super) fn lock(&self) -> &'a Lock<R, G> {
        self.lock
    }
}

impl<R: RawLock, G> Guard<'_, R, G> {
    pub fn protected(&mut self) -> &mut G {
        self
    }
}

impl<R: RawLock, G> Deref for Guard<'_, R, G> {
    type Target = G;

    fn deref(&self) -> &G {
        unsafe { &*self.lock.protected.get() }
    }
}

impl<R: RawLock, G> DerefMut for Guard<'_, R, G> {
    fn deref_mut(&mut self) -> &mut G {
        unsafe { &mut *self.lock.protected.get() }
    }
}

impl<R: RawLock, G> Drop for Guard<'_, R, G> {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            unsafe { R::unlock(self.lock.raw.get(), state) }
        }
    }
}

#[must_use = "the lock is released as soon as the guard is dropped"]
pub struct ReadGuard<'a, R: RawRwLock, G = ()> {
    lock:      &'a Lock<R, G>,
    // an Option only to be taken in drop()
    state:     Option<R::GuardState>,
    _not_send: PhantomData<*mut ()>,
}

impl<R: RawRwLock, G> ReadGuard<'_, R, G> {
    pub fn protected(&self) -> &G {
        self
    }
}

impl<R: RawRwLock, G> Deref for ReadGuard<'_, R, G> {
    type Target = G;

    fn deref(&self) -> &G {
        unsafe { &*self.lock.protected.get() }
    }
}

impl<R: RawRwLock, G> Drop for ReadGuard<'_, R, G> {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            unsafe { R::unlock_shared(self.lock.raw.get(), state) }
        }
    }
}

/// A Lock initializer: new_lock!(g) or new_lock!(g, "name") - over @g, with a
/// lockdep class of its own, named @name or for the call site.
#[macro_export]
macro_rules! new_lock {
    (@init $g:expr, $name:expr) => {{
        #[cfg(kernel)]
        let key = ::kernel::static_lock_class!().as_ptr().cast();
        #[cfg(not(kernel))]
        let key = ::core::ptr::null_mut();

        let g = $g;
        let name = unsafe { ::core::ffi::CStr::from_bytes_with_nul_unchecked($name.as_bytes()) };
        unsafe { $crate::locking::Lock::new(g, name, key) }
    }};
    ($g:expr $(,)?) => {
        $crate::new_lock!(@init $g, ::core::concat!(::core::file!(), ":", ::core::line!(), "\0"))
    };
    ($g:expr, $name:literal $(,)?) => {
        $crate::new_lock!(@init $g, ::core::concat!($name, "\0"))
    };
}
