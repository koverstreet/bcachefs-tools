// SPDX-License-Identifier: GPL-2.0

//! Locks for code shared by both builds: the kernel's kernel::sync in-kernel,
//! bcachefs-shim's copy of the same API in userspace - pinned, initialized in
//! place: new_mutex!() is a pin-init initializer, a field's #[init(...)].
//!
//! RwSem is ours in-kernel: the kernel crate has no reader/writer lock - its
//! Lock<T, Backend> only hands out exclusive guards.

#[cfg(kernel)]
pub use kernel::sync::{new_mutex, Mutex};

#[cfg(not(kernel))]
pub use bcachefs_shim::sync::{new_mutex, Mutex};

#[cfg(not(kernel))]
pub use bcachefs_shim::sync::{new_rwsem, RwSem, RwSemReadGuard, RwSemWriteGuard};

#[cfg(kernel)]
pub use crate::new_rwsem;

#[cfg(kernel)]
pub use rwsem::{RwSem, RwSemReadGuard, RwSemWriteGuard};

#[cfg(kernel)]
mod rwsem {
    use crate::c;
    use core::cell::UnsafeCell;
    use core::marker::{PhantomData, PhantomPinned};
    use core::ops::{Deref, DerefMut};
    use core::pin::Pin;

    /// A reader/writer semaphore over a T: the kernel's rw_semaphore - readers
    /// share it, a writer has it alone, and either may sleep holding it.
    pub struct RwSem<T> {
        sem:  UnsafeCell<c::rw_semaphore>,
        data: UnsafeCell<T>,
        _pin: PhantomPinned,
    }

    // SAFETY: as std's RwLock - a writer may be on any thread, so T: Send;
    // readers share &T across threads, so T: Sync.
    unsafe impl<T: Send> Send for RwSem<T> {}
    unsafe impl<T: Send + Sync> Sync for RwSem<T> {}

    impl<T> RwSem<T> {
        /// init_rwsem(), with a lockdep class of its own - @key, named
        /// @name. new_rwsem!() is this, with a static key per call site.
        #[doc(hidden)]
        pub fn new(
            t:    T,
            name: &'static core::ffi::CStr,
            key:  Pin<&'static kernel::sync::LockClassKey>,
        ) -> impl ::pin_init::PinInit<Self> {
            // SAFETY: __init_rwsem() initializes the semaphore in place - it's
            // pinned - and the value is written; that's all of it.
            unsafe {
                ::pin_init::pin_init_from_closure(move |slot: *mut Self| {
                    c::__init_rwsem((&raw mut (*slot).sem).cast(), name.as_ptr().cast(),
                                    key.as_ptr());
                    (&raw mut (*slot).data).write(UnsafeCell::new(t));
                    Ok(())
                })
            }
        }

        /// down_read(): shared with other readers.
        pub fn read(&self) -> RwSemReadGuard<'_, T> {
            unsafe { c::down_read(self.sem.get()) };
            RwSemReadGuard(self, PhantomData)
        }

        /// down_write(): alone.
        pub fn write(&self) -> RwSemWriteGuard<'_, T> {
            unsafe { c::down_write(self.sem.get()) };
            RwSemWriteGuard(self, PhantomData)
        }
    }

    /// Not Send: the guards are released by the task that took them, as
    /// lockdep checks - as the kernel crate's are.
    type NotSend = PhantomData<*mut ()>;

    /// A read-held RwSem: up_read() when dropped.
    pub struct RwSemReadGuard<'a, T>(&'a RwSem<T>, NotSend);

    impl<T> Deref for RwSemReadGuard<'_, T> {
        type Target = T;

        fn deref(&self) -> &T {
            // SAFETY: read-held - writers are excluded
            unsafe { &*self.0.data.get() }
        }
    }

    impl<T> Drop for RwSemReadGuard<'_, T> {
        fn drop(&mut self) {
            unsafe { c::up_read(self.0.sem.get()) }
        }
    }

    /// A write-held RwSem: up_write() when dropped.
    pub struct RwSemWriteGuard<'a, T>(&'a RwSem<T>, NotSend);

    impl<T> Deref for RwSemWriteGuard<'_, T> {
        type Target = T;

        fn deref(&self) -> &T {
            // SAFETY: write-held - everyone else is excluded
            unsafe { &*self.0.data.get() }
        }
    }

    impl<T> DerefMut for RwSemWriteGuard<'_, T> {
        fn deref_mut(&mut self) -> &mut T {
            // SAFETY: write-held - everyone else is excluded
            unsafe { &mut *self.0.data.get() }
        }
    }

    impl<T> Drop for RwSemWriteGuard<'_, T> {
        fn drop(&mut self) {
            unsafe { c::up_write(self.0.sem.get()) }
        }
    }
}

/// An RwSem initializer for @inner: as new_mutex!(), the name optional -
/// lockdep's, the call site's if not given.
#[cfg(kernel)]
#[macro_export]
macro_rules! new_rwsem {
    ($inner:expr $(,)?) => {
        $crate::new_rwsem!(@name $inner, ::core::concat!(::core::file!(), ":", ::core::line!(), "\0"))
    };
    ($inner:expr, $name:literal $(,)?) => {
        $crate::new_rwsem!(@name $inner, ::core::concat!($name, "\0"))
    };
    (@name $inner:expr, $name:expr) => {
        $crate::util::sync::RwSem::new(
            $inner,
            // SAFETY: NUL-terminated, no NUL before it - literals' text
            unsafe { ::core::ffi::CStr::from_bytes_with_nul_unchecked($name.as_bytes()) },
            ::kernel::static_lock_class!())
    };
}
