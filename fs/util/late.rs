// SPDX-License-Identifier: GPL-2.0

//! Late<T>: a filesystem's, made when it goes read-write, in place and with
//! C's layout: T says whether it's been made, as a pointer's null or a
//! mempool's elements do, so there's no flag for C to see. Published, as
//! LockedBy's published half is: anyone's to read, made under the state lock.
//! Using it before it's made panics; that nothing does is the code around it's
//! to guarantee, as it was C's.

use core::cell::UnsafeCell;
use pin_init::PinInit;

use crate::fs::StateLockWrite;
use crate::util::alloc::init_at;

/// Zeroed is not yet made, and what's not made owns nothing.
///
/// # Safety
/// is_made() is false for zeroed, and Drop of an unmade T does nothing.
pub unsafe trait LateInit: ::pin_init::Zeroable {
    fn is_made(&self) -> bool;
}

#[repr(transparent)]
pub struct Late<T: LateInit>(UnsafeCell<T>);

unsafe impl<T: LateInit + Sync> Sync for Late<T> {}
unsafe impl<T: LateInit + Send> Send for Late<T> {}
unsafe impl<T: LateInit> ::pin_init::Zeroable for Late<T> {}

impl<T: LateInit> Late<T> {
    /// Make it with @init's initializer, unless it's made: on failure, it's
    /// unmade still.
    pub fn init<I, E>(&self, _: &StateLockWrite<'_>, init: impl FnOnce() -> I) -> Result<(), E>
    where
        I: PinInit<T, E>,
    {
        let slot = self.0.get();
        if unsafe { (*slot).is_made() } {
            return Ok(());
        }
        unsafe { init_at(init(), slot).inspect_err(|_| core::ptr::write_bytes(slot, 0, 1)) }
    }

    pub fn is_made(&self) -> bool {
        unsafe { (*self.0.get()).is_made() }
    }

    /// Dropped, and unmade again
    ///
    /// # Safety
    /// Nothing uses it any more.
    pub unsafe fn unmake(&self) {
        let slot = self.0.get();
        unsafe {
            core::ptr::drop_in_place(slot);
            core::ptr::write_bytes(slot, 0, 1);
        }
    }
}

impl<T: LateInit> core::ops::Deref for Late<T> {
    type Target = T;

    #[track_caller]
    fn deref(&self) -> &T {
        let t = unsafe { &*self.0.get() };
        assert!(t.is_made(), "{} used before it was made", core::any::type_name::<T>());
        t
    }
}
