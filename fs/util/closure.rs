// SPDX-License-Identifier: GPL-2.0

//! Refs on a closure (vendor/closure.h): closure_return_sync() waits for them,
//! so a walker can use what it found after dropping the lock it found it under.

use crate::c;

/// A ref on a closure, put on drop: closure_put().
#[must_use = "the ref is put as soon as it's dropped"]
pub struct ClosureRef<'a>(&'a c::closure);

/// closure_get_not_zero(): a ref, unless the closure has none left - it's
/// finished, or finishing.
pub fn closure_get_not_zero(cl: &c::closure) -> Option<ClosureRef<'_>> {
    // SAFETY: a closure's refcount is atomic, changed through a shared pointer
    unsafe { c::closure_get_not_zero(core::ptr::from_ref(cl).cast_mut()) }
        .then(|| ClosureRef(cl))
}

impl Drop for ClosureRef<'_> {
    fn drop(&mut self) {
        // SAFETY: the ref this holds
        unsafe { c::closure_put(core::ptr::from_ref(self.0).cast_mut()) }
    }
}
