// SPDX-License-Identifier: GPL-2.0

//! Locks for code shared by both builds: the kernel's kernel::sync in-kernel,
//! bcachefs-shim's copy of the same API in userspace - pinned, initialized in
//! place: new_mutex!() is a pin-init initializer, a field's #[init(...)].
//!
//! RwSem is ours in-kernel: the kernel crate has no reader/writer lock - its
//! Lock<T, Backend> only hands out exclusive guards. SeqLock is ours in both
//! builds - the kernel crate has none, and it's only atomics. PercpuRwSem is
//! ours in both too, over C's struct in both: C takes it as well.

#[cfg(kernel)]
pub use kernel::sync::{new_mutex, Mutex, MutexGuard};

#[cfg(not(kernel))]
pub use bcachefs_shim::sync::{new_mutex, Mutex, MutexGuard};

#[cfg(not(kernel))]
pub use bcachefs_shim::sync::{new_rwsem, RwSem, RwSemReadGuard, RwSemWriteGuard};

#[cfg(kernel)]
pub use crate::new_rwsem;

#[cfg(kernel)]
pub use rwsem::{RwSem, RwSemReadGuard, RwSemWriteGuard};

pub use seqlock::SeqLock;

pub use percpu_rwsem::{PercpuRwSem, PercpuRwSemReadGuard, PercpuRwSemWriteGuard};

mod percpu_rwsem {
    use crate::c;
    use crate::errcode::{ret_to_result_void, BchError};
    use core::cell::UnsafeCell;
    use core::ffi::{c_void, CStr};
    use core::marker::{PhantomData, PhantomPinned};

    /// The kernel's percpu_rw_semaphore, as C's struct: readers - many and
    /// cheap, a per-CPU count each - share it, a writer has it alone, at the
    /// cost of waiting out every reader. It guards no Rust data: what it
    /// orders is C's as much as Rust's, and C takes it through the same
    /// struct.
    #[repr(transparent)]
    pub struct PercpuRwSem {
        sem:  UnsafeCell<c::percpu_rw_semaphore>,
        _pin: PhantomPinned,
    }

    // SAFETY: a lock, taken and released through C
    unsafe impl Send for PercpuRwSem {}
    unsafe impl Sync for PercpuRwSem {}

    impl PercpuRwSem {
        /// percpu_init_rwsem(), with a lockdep class of its own - @key, named
        /// @name. new_percpu_rwsem!() is this, with a static key per call
        /// site. Fails if the per-CPU counts can't be allocated.
        #[doc(hidden)]
        pub fn new(name: &'static CStr, key: *mut c_void) -> impl ::pin_init::PinInit<Self, BchError> {
            // SAFETY: rust_percpu_init_rwsem() initializes the semaphore in
            // place - it's pinned - and that's all of it
            unsafe {
                ::pin_init::pin_init_from_closure(move |slot: *mut Self| {
                    ret_to_result_void(c::rust_percpu_init_rwsem((&raw mut (*slot).sem).cast(),
                                                                 name.as_ptr().cast(), key))
                })
            }
        }

        /// percpu_down_read(): shared with other readers.
        pub fn read(&self) -> PercpuRwSemReadGuard<'_> {
            unsafe { c::rust_percpu_down_read(self.sem.get()) };
            PercpuRwSemReadGuard(self, PhantomData)
        }

        /// percpu_down_write(): alone, once every reader has gone.
        pub fn write(&self) -> PercpuRwSemWriteGuard<'_> {
            unsafe { c::rust_percpu_down_write(self.sem.get()) };
            PercpuRwSemWriteGuard(self, PhantomData)
        }
    }

    impl Drop for PercpuRwSem {
        fn drop(&mut self) {
            unsafe { c::rust_percpu_free_rwsem(self.sem.get_mut()) }
        }
    }

    /// Not Send: released by the task that took it, as lockdep checks.
    type NotSend = PhantomData<*mut ()>;

    /// A read-held PercpuRwSem: percpu_up_read() when dropped.
    pub struct PercpuRwSemReadGuard<'a>(&'a PercpuRwSem, NotSend);

    impl Drop for PercpuRwSemReadGuard<'_> {
        fn drop(&mut self) {
            unsafe { c::rust_percpu_up_read(self.0.sem.get()) }
        }
    }

    /// A write-held PercpuRwSem: percpu_up_write() when dropped.
    pub struct PercpuRwSemWriteGuard<'a>(&'a PercpuRwSem, NotSend);

    impl Drop for PercpuRwSemWriteGuard<'_> {
        fn drop(&mut self) {
            unsafe { c::rust_percpu_up_write(self.0.sem.get()) }
        }
    }
}

/// A PercpuRwSem initializer, named @name for lockdep, with a lock class of
/// its own.
#[macro_export]
macro_rules! new_percpu_rwsem {
    ($name:literal $(,)?) => {{
        #[cfg(kernel)]
        let key = ::kernel::static_lock_class!().as_ptr().cast();
        #[cfg(not(kernel))]
        let key = ::core::ptr::null_mut();

        $crate::util::sync::PercpuRwSem::new(
            // SAFETY: NUL-terminated, no NUL before it - a literal's text
            unsafe { ::core::ffi::CStr::from_bytes_with_nul_unchecked(
                ::core::concat!($name, "\0").as_bytes()) },
            key)
    }};
}

pub use crate::new_percpu_rwsem;

mod seqlock {
    use core::cell::UnsafeCell;
    use core::hint::spin_loop;
    use core::mem::MaybeUninit;
    use core::sync::atomic::{fence, AtomicU32, Ordering};

    /// A sequence lock over a T: readers copy the value out without taking
    /// anything, and retry a copy a write overlapped; writers have it alone.
    /// For small state read rarely and written often - a progress indicator
    /// sysfs shows while its work updates it.
    ///
    /// The sequence is odd while a write is in progress: taking it from even
    /// to odd is the writers' lock, so there's no lock besides it, and a
    /// second writer spins. A write nested in a write deadlocks.
    ///
    /// The reader's copy races with the writer's stores, as C's seqcount
    /// readers' do: it's volatile - the kernel's READ_ONCE() - into a
    /// MaybeUninit, and becomes a T only once the sequence says no write
    /// overlapped it, so a torn copy (half a bbpos, a bool mid-store) is
    /// never a T at all. Rust's memory model would want atomic word-by-word
    /// copies for that race; the kernel's, which this follows, doesn't.
    #[repr(C)]
    pub struct SeqLock<T: Copy> {
        seq:  AtomicU32,
        data: UnsafeCell<T>,
    }

    // SAFETY: a reader gets a copy of the T, on any thread - T: Send; the T
    // itself is only ever mutated by the one writer.
    unsafe impl<T: Copy + Send> Sync for SeqLock<T> {}

    // SAFETY: a zero sequence is even - unlocked - and a zeroed T is a T.
    unsafe impl<T: Copy + ::pin_init::Zeroable> ::pin_init::Zeroable for SeqLock<T> {}

    impl<T: Copy> SeqLock<T> {
        pub const fn new(v: T) -> Self {
            Self { seq: AtomicU32::new(0), data: UnsafeCell::new(v) }
        }

        /// The value, as of the last write to finish.
        pub fn read(&self) -> T {
            loop {
                let seq = self.seq.load(Ordering::Acquire);
                if seq & 1 != 0 {
                    spin_loop();
                    continue;
                }

                // SAFETY: valid for reads; any bytes are a MaybeUninit
                let v = unsafe { self.data.get().cast::<MaybeUninit<T>>().read_volatile() };

                fence(Ordering::Acquire);
                if self.seq.load(Ordering::Relaxed) == seq {
                    // SAFETY: no write overlapped the copy - it's a T, as the
                    // last write left it
                    return unsafe { v.assume_init() };
                }
            }
        }

        /// @f on the value, readers held off - shown what @f leaves, once it
        /// returns. A panic in @f leaves the sequence odd: write-held, so
        /// readers spin on it for good.
        pub fn write<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
            let mut seq = self.seq.load(Ordering::Relaxed);
            loop {
                if seq & 1 != 0 {
                    spin_loop();
                    seq = self.seq.load(Ordering::Relaxed);
                    continue;
                }
                match self.seq.compare_exchange_weak(seq, seq.wrapping_add(1),
                                                     Ordering::Acquire, Ordering::Relaxed) {
                    Ok(_)    => break,
                    Err(now) => seq = now,
                }
            }
            // The odd sequence, before any of the value's stores:
            fence(Ordering::Release);

            // SAFETY: the odd sequence is ours - no other writer; readers
            // discard what they copy meanwhile
            let ret = f(unsafe { &mut *self.data.get() });

            self.seq.store(seq.wrapping_add(2), Ordering::Release);
            ret
        }
    }

    // A torn read is silent - a copy that's half one write and half another -
    // so: a writer keeps both halves equal, readers check every copy.
    #[cfg(test)]
    mod tests {
        use super::SeqLock;
        use std::sync::atomic::{AtomicBool, Ordering};

        #[test]
        fn no_torn_reads() {
            let l = SeqLock::new([0u64; 4]);
            let done = AtomicBool::new(false);

            std::thread::scope(|s| {
                for _ in 0..3 {
                    s.spawn(|| {
                        let mut reads = 0u64;
                        while !done.load(Ordering::Relaxed) || reads < 1000 {
                            let v = l.read();
                            assert!(v.iter().all(|&x| x == v[0]), "torn read: {v:?}");
                            reads += 1;
                        }
                    });
                }
                let writers: Vec<_> = (0..2).map(|_| s.spawn(|| {
                    for i in 0..200_000u64 {
                        l.write(|v| *v = [i; 4]);
                    }
                })).collect();
                writers.into_iter().for_each(|w| w.join().unwrap());
                done.store(true, Ordering::Relaxed);
            });
        }
    }
}

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
