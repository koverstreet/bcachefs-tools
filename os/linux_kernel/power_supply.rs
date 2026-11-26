// SPDX-License-Identifier: GPL-2.0

//! The power supply class: whether we're on battery, and a notifier told
//! when a supply changes.

#[path = "../power_supply.rs"]
mod common;
pub(crate) use common::*;

#[cfg(CONFIG_POWER_SUPPLY)]
pub(crate) use supplied::*;

#[cfg(CONFIG_POWER_SUPPLY)]
mod supplied {
    use super::Watch;
    use crate::c;
    use crate::errcode::{ret_to_result_void, BchError};
    use core::cell::UnsafeCell;
    use core::ffi::{c_int, c_void};
    use core::sync::atomic::{AtomicPtr, Ordering::{AcqRel, Acquire, Relaxed}};
    use crate::util::ffi::c_ulong;

    pub(crate) fn on_battery() -> bool {
        unsafe { c::power_supply_is_system_supplied() == 0 }
    }

    /// The notifier_block the class links into its chain, first: the
    /// callback finds the rest from it. @owner is null unregistered.
    #[repr(C)]
    pub(crate) struct PowerNotifier<T> {
        nb:    UnsafeCell<c::notifier_block>,
        owner: AtomicPtr<T>,
    }

    // SAFETY: unregistered, with no callback
    unsafe impl<T> pin_init::Zeroable for PowerNotifier<T> {}

    // SAFETY: @nb is written only unregistered, by register(); the chain's
    // lock orders it before the callback's reads
    unsafe impl<T: Sync> Sync for PowerNotifier<T> {}

    impl<T: Watch> PowerNotifier<T> {
        pub(crate) fn register(owner: &T) -> Result<(), BchError> {
            let n = owner.power_notifier();
            if n.owner.compare_exchange(core::ptr::null_mut(), core::ptr::from_ref(owner).cast_mut(),
                                        AcqRel, Relaxed).is_err() {
                panic!("a power notifier registered twice");
            }

            unsafe { (*n.nb.get()).notifier_call = Some(notify::<T>) };
            ret_to_result_void(unsafe { c::power_supply_reg_notifier(n.nb.get()) })
                .inspect_err(|_| n.owner.store(core::ptr::null_mut(), Relaxed))
        }

        /// Unregistered, any callback still running waited out.
        ///
        /// # Safety
        /// Nothing else uses it meanwhile: the owner's placed_drop() calls this.
        pub(crate) unsafe fn unregister(&self) {
            if !self.owner.swap(core::ptr::null_mut(), Acquire).is_null() {
                unsafe { c::power_supply_unreg_notifier(self.nb.get()) };
            }
        }
    }

    impl<T> Drop for PowerNotifier<T> {
        fn drop(&mut self) {
            assert!(self.owner.get_mut().is_null(),
                    "a power notifier dropped registered: its owner's placed_drop() wasn't called");
        }
    }

    unsafe extern "C" fn notify<T: Watch>(nb: *mut c::notifier_block, _event: c_ulong, _data: *mut c_void) -> c_int {
        let n = unsafe { &*nb.cast::<PowerNotifier<T>>() };
        let owner = unsafe { &*n.owner.load(Acquire) };
        owner.power_changed();
        c::NOTIFY_OK
    }
}
