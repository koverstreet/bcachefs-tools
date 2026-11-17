// SPDX-License-Identifier: GPL-2.0

//! What both ports' power_supply.rs have: a Watch, and the nothing to watch
//! without the kernel's power supply class - never on battery.

use crate::util::place::{Contained, PlacedDrop};

#[cfg(all(kernel, CONFIG_POWER_SUPPLY))]
use super::PowerNotifier;

/// What a PowerNotifier tells, from any thread while it's registered. Placed,
/// so the owner the callback is given stays where it is, as long as what it's
/// in: its PlacedDrop unregisters it, while what power_changed() uses is
/// still there.
// with nothing to watch, nothing calls back
#[cfg_attr(not(all(kernel, CONFIG_POWER_SUPPLY)), allow(dead_code))]
pub(crate) trait Watch: Contained + PlacedDrop + Sync {
    fn power_notifier(&self) -> &PowerNotifier<Self>;

    /// A power supply changed: on_battery() may have.
    fn power_changed(&self);
}

#[cfg(not(all(kernel, CONFIG_POWER_SUPPLY)))]
pub(crate) use unsupplied::*;

#[cfg(not(all(kernel, CONFIG_POWER_SUPPLY)))]
mod unsupplied {
    use super::Watch;
    use crate::errcode::BchError;
    use core::marker::PhantomData;

    pub(crate) fn on_battery() -> bool {
        false
    }

    pub(crate) struct PowerNotifier<T>(PhantomData<fn(&T)>);

    // SAFETY: nothing in it
    unsafe impl<T> pin_init::Zeroable for PowerNotifier<T> {}

    impl<T: Watch> PowerNotifier<T> {
        pub(crate) fn register(_owner: &T) -> Result<(), BchError> {
            Ok(())
        }

        /// # Safety
        /// As the kernel's.
        pub(crate) unsafe fn unregister(&self) {}
    }
}
