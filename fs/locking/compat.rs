// SPDX-License-Identifier: GPL-2.0

//! What exists because C owns the lock - to go as the C does: a C struct's
//! lock borrowed as a Locked.

use crate::locking::{Locked, RawLockBase};

impl<R: RawLockBase> Locked<R> {
    /// A lock in a C struct - C's to initialize and free - as a Locked over
    /// nothing, taken as C takes it.
    pub fn from_c(raw: &R) -> &Self {
        const { assert!(size_of::<Self>() == size_of::<R>() && align_of::<Self>() == align_of::<R>()) };
        // Taking it through a & writes it: a RawLockBase's state is
        // interior-mutable. Borrowed, so never dropped - C frees it.
        unsafe { &*core::ptr::from_ref(raw).cast() }
    }
}
