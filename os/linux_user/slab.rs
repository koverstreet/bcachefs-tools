// SPDX-License-Identifier: GPL-2.0

//! The kernel's slab cache of @T's, in userspace: which has no slab caches,
//! its @T's plain allocations.

use crate::util::alloc::{kfree, kzalloc, Alloc, AllocError, Allocator, Flags, Kmalloc};
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

    /// The Allocator's: @layout, if it's within T's.
    fn alloc(&self, layout: Layout, zeroed: bool) -> Result<NonNull<[u8]>, AllocError> {
        let t = Layout::new::<T>();
        if layout.size() > t.size() || layout.align() > t.align() {
            return Err(AllocError);
        }
        if zeroed { Kmalloc.allocate_zeroed(t) } else { Kmalloc.allocate(t) }
    }
}

/// An allocator of a @T's memory: of no more than its layout.
unsafe impl<T> Allocator for KmemCache<T> {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        self.alloc(layout, false)
    }

    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        self.alloc(layout, true)
    }

    unsafe fn deallocate(&self, p: NonNull<u8>, _: Layout) {
        unsafe { self.free(p.cast()) }
    }
}

unsafe impl<T> Alloc for KmemCache<T> {}
