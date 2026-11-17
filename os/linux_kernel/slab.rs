// SPDX-License-Identifier: GPL-2.0

//! A slab cache of @T's - kmem_cache_create()'s - for what's allocated and
//! freed often enough to want its own. A static: made at module init and
//! destroyed at exit, with nothing of it allocated before or after.

use crate::c;
use crate::util::alloc::{flags, Alloc, AllocError, Allocator, Flags};
use core::alloc::Layout;
use core::ffi::CStr;
use core::marker::PhantomData;
use core::ptr::{null_mut, NonNull};
use core::sync::atomic::AtomicPtr;
use core::sync::atomic::Ordering::{Acquire, Release};

pub struct KmemCache<T> {
    cache: AtomicPtr<c::kmem_cache>,
    _t:    PhantomData<fn() -> T>,
}

impl<T> KmemCache<T> {
    pub const fn new() -> Self {
        Self { cache: AtomicPtr::new(null_mut()), _t: PhantomData }
    }

    /// Made, as KMEM_CACHE() makes one: @name, of @T's size and alignment,
    /// with @flags - SLAB_*.
    pub fn create(&self, name: &'static CStr, flags: c::slab_flags_t) -> Result<(), AllocError> {
        // plain data, all zeroes the defaults
        let mut args: c::kmem_cache_args = ::pin_init::zeroed();
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
    pub unsafe fn destroy(&self) {
        let p = self.cache.swap(null_mut(), Acquire);
        if !p.is_null() {
            unsafe { c::kmem_cache_destroy(p) }
        }
    }

    pub fn as_ptr(&self) -> *mut c::kmem_cache {
        let p = self.cache.load(Acquire);
        debug_assert!(!p.is_null(), "a KmemCache used before its create(), or after its destroy()");
        p
    }

    /// A @T's memory, zeroed.
    pub fn zalloc(&self, gfp: Flags) -> Option<NonNull<T>> {
        NonNull::new(unsafe {
            c::kmem_cache_alloc_noprof(self.as_ptr(), crate::util::alloc::current_gfp_context_raw((gfp | flags::ZERO).raw()))
        }.cast())
    }

    /// A @T's memory, uninitialized, charged to @lru - a superblock's inode
    /// LRU, for alloc_inode_sb().
    pub fn alloc_lru(&self, lru: *mut c::list_lru, gfp: Flags) -> Option<NonNull<T>> {
        NonNull::new(unsafe {
            c::kmem_cache_alloc_lru_noprof(self.as_ptr(), lru, crate::util::alloc::current_gfp_context_raw(gfp.raw()))
        }.cast())
    }

    /// # Safety
    /// @p is this cache's, and nothing uses it after.
    pub unsafe fn free(&self, p: NonNull<T>) {
        unsafe { c::kmem_cache_free(self.as_ptr(), p.as_ptr().cast()) }
    }

    /// The Allocator's: @layout, if it's within T's.
    fn alloc(&self, layout: Layout, zeroed: bool) -> Result<NonNull<[u8]>, AllocError> {
        let t = Layout::new::<T>();
        if layout.size() > t.size() || layout.align() > t.align() {
            return Err(AllocError);
        }

        let gfp = if zeroed { flags::ZERO } else { Flags::empty() };
        let p = NonNull::new(unsafe {
            c::kmem_cache_alloc_noprof(self.as_ptr(), crate::util::alloc::current_gfp_context_raw(gfp.raw()))
        }).ok_or(AllocError)?;
        Ok(NonNull::slice_from_raw_parts(p.cast(), t.size()))
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
