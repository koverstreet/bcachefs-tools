// SPDX-License-Identifier: GPL-2.0

use super::Srcu;
use core::ffi::c_int;

#[must_use = "the SRCU read section ends as soon as the guard is dropped"]
pub struct SrcuReadGuard<'a> {
    srcu: &'a Srcu,
    idx:  c_int,
}

impl<'a> SrcuReadGuard<'a> {
    pub fn srcu(&self) -> &'a Srcu {
        self.srcu
    }

    /// The section left held, for what holds it where a guard can't be - a
    /// transaction's, across its unlocks: its index, for from_raw().
    pub fn into_raw(self) -> c_int {
        core::mem::ManuallyDrop::new(self).idx
    }

    /// # Safety
    /// @idx is into_raw()'s, of a read_lock() of @srcu, taken back once.
    pub unsafe fn from_raw(srcu: &'a Srcu, idx: c_int) -> Self {
        SrcuReadGuard { srcu, idx }
    }
}

impl Drop for SrcuReadGuard<'_> {
    fn drop(&mut self) {
        self.srcu.read_unlock(self.idx);
    }
}
