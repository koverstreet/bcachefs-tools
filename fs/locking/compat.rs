// SPDX-License-Identifier: GPL-2.0

//! What exists because C owns the lock, or holds it calling in - to go as the
//! C does: a C struct's lock borrowed as a Lock, and a Lock whose caller
//! already holds it.

use super::{Guard, Lock, RawLock};
use core::mem::{align_of, size_of};

impl<R: RawLock> Lock<R> {
    /// A lock in a C struct - C's to initialize and free - as a Lock over
    /// nothing, taken as C takes it.
    pub fn from_c(raw: &R) -> &Self {
        const { assert!(size_of::<Self>() == size_of::<R>() && align_of::<Self>() == align_of::<R>()) };
        // Taking it through a & writes it: sound because C's locks are
        // Opaques (raw.rs). Borrowed, so never dropped - C frees it.
        unsafe { &*core::ptr::from_ref(raw).cast() }
    }
}

impl<R: RawLock, G> Lock<R, G> {
    /// The lock, as a caller holds it - C's, calling in: its guard isn't ours
    /// to release.
    ///
    /// # Safety
    /// The caller holds the lock exclusively - not as one of its readers -
    /// and holds it for as long as this guard lives; nothing else this task
    /// holds has the G.
    pub unsafe fn held(&self) -> Guard<'_, R, G> {
        self.assert_held();
        unsafe { Guard::from_held(self, None) }
    }
}
