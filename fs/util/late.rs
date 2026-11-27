// SPDX-License-Identifier: GPL-2.0

//! Late<T, L>: a filesystem's, made after it is - when it goes read-write, or
//! a compression type is first wanted - or a module's static, made at its
//! init (ModuleInit); in place and with C's layout: T says
//! whether it's been made, as a pointer's null or a mempool's elements do, so
//! there's no flag for C to see. Published, as LockedBy's published half is:
//! anyone's to read, made under lock L. Using it before it's made panics; that
//! nothing does is the code around it's to guarantee, as it was C's.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use pin_init::PinInit;

use crate::os::module::{Quiesced, ThisModule};
use crate::util::alloc::init_at;

/// Zeroed is not yet made, and what's not made owns nothing: is_made() is
/// false for zeroed, and Drop of an unmade T does nothing.
pub trait LateInit: ::pin_init::Zeroable {
    fn is_made(&self) -> bool;
}

/// What a made T does as its filesystem stops, before teardown frees what its
/// work uses: a workqueue drains, so its destroy, as it drops, has nothing
/// left to run.
pub trait LateStop {
    fn stop(&self) {}
}

/// The lock a Late is made under: init() takes its guard.
pub trait LateLock {
    type Guard<'a>;
}

pub enum StateLock {}
impl LateLock for StateLock {
    type Guard<'a> = crate::fs::StateLockWrite<'a>;
}

pub enum SbLock {}
impl LateLock for SbLock {
    type Guard<'a> = crate::fs::SbLockGuard<'a>;
}

/// A module's static: made at module init, as nothing else of ours runs, and
/// dropped at its exit - exit().
pub enum ModuleInit {}
impl LateLock for ModuleInit {
    type Guard<'a> = ThisModule;
}

#[repr(transparent)]
pub struct Late<T: LateInit, L: LateLock = StateLock>(UnsafeCell<T>, PhantomData<L>);

unsafe impl<T: LateInit + Sync, L: LateLock> Sync for Late<T, L> {}
unsafe impl<T: LateInit + Send, L: LateLock> Send for Late<T, L> {}
unsafe impl<T: LateInit, L: LateLock> ::pin_init::Zeroable for Late<T, L> {}

impl<T: LateInit, L: LateLock> Late<T, L> {
    /// Make it with @init's initializer, unless it's made: on failure, it's
    /// unmade still.
    pub fn init<I, E>(&self, _: &L::Guard<'_>, init: impl FnOnce() -> I) -> Result<(), E>
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

    /// Its T stopped, if it's made: see LateStop.
    pub fn stop(&self) where T: LateStop {
        if self.is_made() {
            (**self).stop()
        }
    }
}

impl<T: LateInit> Late<T, ModuleInit> {
    /// Dropped, and unmade again: a no-op on one never made.
    pub fn exit(&self, _: &Quiesced<'_>) {
        if self.is_made() {
            let slot = self.0.get();
            unsafe {
                core::ptr::drop_in_place(slot);
                core::ptr::write_bytes(slot, 0, 1);
            }
        }
    }
}

impl<T: LateInit, L: LateLock> core::ops::Deref for Late<T, L> {
    type Target = T;

    #[track_caller]
    fn deref(&self) -> &T {
        let t = unsafe { &*self.0.get() };
        assert!(t.is_made(), "{} used before it was made", core::any::type_name::<T>());
        t
    }
}
