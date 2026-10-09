// SPDX-License-Identifier: GPL-2.0

//! The RCU read side, for Rust reading data C publishes under RCU.
//!
//! read_lock() is rcu_read_lock() until the guard drops: the kernel's, or in
//! userspace liburcu's - the memb flavor, which the tools link and C's
//! rcu_read_lock() is.
//!
//! dereference() is rcu_dereference(), and unsafe: the guard only says a
//! read-side section is open, not that the pointer is to RCU-protected data,
//! or that what it points to is what the caller says. Its callers are the
//! places that read a C structure under RCU directly; each is unfinished
//! conversion, to come back to when the structure is a Rust one.

use core::marker::PhantomData;
use core::sync::atomic::{AtomicPtr, Ordering};

#[cfg(not(kernel))]
unsafe extern "C" {
    fn urcu_memb_read_lock();
    fn urcu_memb_read_unlock();
}

/// An RCU read-side critical section, open until dropped. Not Send: it's
/// the thread's.
pub struct ReadGuard {
    #[cfg(kernel)]
    _guard:   kernel::sync::rcu::Guard,
    _not_send: PhantomData<*mut ()>,
}

/// rcu_read_lock(), until the guard drops.
pub fn read_lock() -> ReadGuard {
    #[cfg(not(kernel))]
    unsafe { urcu_memb_read_lock() };

    ReadGuard {
        #[cfg(kernel)]
        _guard:   kernel::sync::rcu::read_lock(),
        _not_send: PhantomData,
    }
}

#[cfg(not(kernel))]
impl Drop for ReadGuard {
    fn drop(&mut self) {
        unsafe { urcu_memb_read_unlock() };
    }
}

/// rcu_dereference(*@p): the pointer, as published - good while @_g is.
///
/// # Safety
/// @p is a pointer RCU publishes, and the caller only uses what it points
/// to within @_g's section.
pub unsafe fn dereference<T>(p: *const *mut T, _g: &ReadGuard) -> *mut T {
    unsafe { (*(p as *const AtomicPtr<T>)).load(Ordering::Acquire) }
}
