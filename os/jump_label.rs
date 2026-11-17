// SPDX-License-Identifier: GPL-2.0

//! What both ports' jump_label.rs have: a static key Rust defines.

use core::cell::UnsafeCell;

/// A static key, off till it's enabled: C's DEFINE_STATIC_KEY_FALSE(), in a
/// static - what static_branch_unlikely!(KEY) takes.
#[repr(transparent)]
pub struct StaticKeyFalse(#[doc(hidden)] pub(crate) UnsafeCell<crate::c::static_key_false>);

// SAFETY: only the static key API writes it, which synchronizes itself
unsafe impl Sync for StaticKeyFalse {}

impl StaticKeyFalse {
    /// STATIC_KEY_FALSE_INIT: all zeroes.
    pub const fn new() -> Self {
        StaticKeyFalse(UnsafeCell::new(::pin_init::zeroed()))
    }

    /// For the static key API.
    pub const fn as_ptr(&self) -> *mut crate::c::static_key_false {
        self.0.get()
    }
}
