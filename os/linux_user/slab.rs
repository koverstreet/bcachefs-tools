// SPDX-License-Identifier: GPL-2.0

//! The kernel's slab cache of @T's, in userspace: which has no slab caches,
//! its @T's plain allocations.

use crate::util::alloc::{kfree, kzalloc, Flags};
use core::alloc::Layout;
use core::marker::PhantomData;
use core::ptr::NonNull;

pub struct KmemCache<T>(PhantomData<fn() -> T>);

impl<T> KmemCache<T> {
    pub const fn new() -> Self {
        Self(PhantomData)
    }

    /// A @T's memory, zeroed.
    pub fn zalloc(&self, _gfp: Flags) -> Option<NonNull<T>> {
        kzalloc(Layout::new::<T>()).ok().map(NonNull::cast)
    }

    /// # Safety
    /// @p is this cache's, and nothing uses it after.
    pub unsafe fn free(&self, p: NonNull<T>) {
        unsafe { kfree(p.cast(), Layout::new::<T>()) }
    }
}
