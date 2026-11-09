// SPDX-License-Identifier: GPL-2.0

//! A slab cache of @T's - kmem_cache_create()'s - for what's allocated and
//! freed often enough to want its own. A static: made at module init and
//! destroyed at exit, with nothing of it allocated before or after.
//! Userspace has no slab caches: its @T's are plain allocations.

use crate::c;
#[cfg(kernel)]
use crate::util::alloc::AllocError;
#[cfg(kernel)]
use core::ffi::CStr;
use core::marker::PhantomData;
use core::ptr::{null_mut, NonNull};
use core::sync::atomic::AtomicPtr;
#[cfg(kernel)]
use core::sync::atomic::Ordering::{Acquire, Release};

pub struct KmemCache<T> {
    #[cfg_attr(not(kernel), allow(dead_code))]
    cache: AtomicPtr<c::kmem_cache>,
    _t:    PhantomData<fn() -> T>,
}

impl<T> KmemCache<T> {
    pub const fn new() -> Self {
        Self { cache: AtomicPtr::new(null_mut()), _t: PhantomData }
    }

    /// Made, as KMEM_CACHE() makes one: @name, of @T's size and alignment,
    /// with @flags - SLAB_*.
    #[cfg(kernel)]
    pub fn create(&self, name: &'static CStr, flags: c::slab_flags_t) -> Result<(), AllocError> {
        // SAFETY: plain data, all zeroes the defaults
        let mut args: c::kmem_cache_args = unsafe { core::mem::zeroed() };
        args.align = align_of::<T>() as _;
        let p = unsafe { c::__kmem_cache_create_args(name.as_ptr().cast(), size_of::<T>() as _, &mut args, flags) };
        if p.is_null() {
            return Err(AllocError);
        }
        self.cache.store(p, Release);
        Ok(())
    }

    /// # Safety
    /// Nothing it allocated is still allocated, or will be.
    #[cfg(kernel)]
    pub unsafe fn destroy(&self) {
        let p = self.cache.swap(null_mut(), Acquire);
        if !p.is_null() {
            unsafe { c::kmem_cache_destroy(p) }
        }
    }

    #[cfg(kernel)]
    pub fn as_ptr(&self) -> *mut c::kmem_cache {
        let p = self.cache.load(Acquire);
        debug_assert!(!p.is_null(), "a KmemCache used before its create(), or after its destroy()");
        p
    }

    /// A @T's memory, zeroed.
    pub fn zalloc(&self, gfp: c::gfp_t) -> Option<NonNull<T>> {
        #[cfg(kernel)]
        return NonNull::new(unsafe { c::kmem_cache_alloc_noprof(self.as_ptr(), gfp | c::__GFP_ZERO) }.cast());
        #[cfg(not(kernel))]
        {
            let _ = gfp;
            crate::util::alloc::kzalloc(core::alloc::Layout::new::<T>()).ok().map(NonNull::cast)
        }
    }

    /// A @T's memory, uninitialized, charged to @lru - a superblock's inode
    /// LRU, for alloc_inode_sb().
    #[cfg(kernel)]
    pub fn alloc_lru(&self, lru: *mut c::list_lru, gfp: c::gfp_t) -> Option<NonNull<T>> {
        NonNull::new(unsafe { c::kmem_cache_alloc_lru_noprof(self.as_ptr(), lru, gfp) }.cast())
    }

    /// # Safety
    /// @p is this cache's, and nothing uses it after.
    pub unsafe fn free(&self, p: NonNull<T>) {
        #[cfg(kernel)]
        unsafe { c::kmem_cache_free(self.as_ptr(), p.as_ptr().cast()) }
        #[cfg(not(kernel))]
        unsafe { crate::util::alloc::kfree(p.cast(), core::alloc::Layout::new::<T>()) }
    }
}
