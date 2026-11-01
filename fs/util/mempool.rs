// SPDX-License-Identifier: GPL-2.0

//! The kernel's mempool - userspace's is linux/mempool.c - of kvmalloc()'d
//! elements: mempool_init_kvmalloc_pool(), util.c's element allocator.
//!
//! Its mempool_t is inline, C's layout. Zeroed, it isn't made; made after its
//! owner, it's a Late<Mempool>, whose lock serializes making it. It's exited
//! when it drops, a no-op on one never made. Allocating waits for an element:
//! GFP_KERNEL, the caller's memalloc scope saying the rest.

use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::util::ffi::Opaque;
use crate::util::late::LateInit;
use core::ffi::{c_int, c_void};
use core::ptr::NonNull;
use pin_init::PinInit;

#[repr(transparent)]
pub struct Mempool(Opaque<c::mempool_t>);

// SAFETY: a mempool is every thread's, behind its own lock
unsafe impl Send for Mempool {}
unsafe impl Sync for Mempool {}
// SAFETY: zeroed is not made
unsafe impl ::pin_init::Zeroable for Mempool {}

unsafe impl LateInit for Mempool {
    fn is_made(&self) -> bool {
        unsafe { !(*self.0.as_ptr()).elements.is_null() }
    }
}

impl Mempool {
    /// @min_nr elements of @size bytes. Nothing to undo if it fails.
    pub fn kvmalloc(min_nr: usize, size: usize) -> impl PinInit<Self, BchError> {
        unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut Self| {
                ret_to_result_void(c::mempool_init_noprof(slot.cast(), min_nr as c_int, Some(c::mempool_kvmalloc),
                                                          Some(c::mempool_kvfree), size as *mut c_void))
            })
        }
    }

    /// An element, freed to the pool on drop.
    pub fn alloc(&self) -> PoolBuf<'_> {
        PoolBuf { pool: self, p: self.alloc_raw() }
    }

    /// alloc(), for C to hold: free_raw() takes it back.
    pub fn alloc_raw(&self) -> NonNull<u8> {
        NonNull::new(unsafe { c::mempool_alloc_noprof(self.0.as_ptr(), c::GFP_KERNEL) }.cast())
            .expect("mempool_alloc() waits for an element")
    }

    /// # Safety
    /// @p is this pool's, not used after.
    pub unsafe fn free_raw(&self, p: NonNull<u8>) {
        // SAFETY: the caller's
        unsafe { c::mempool_free(p.as_ptr().cast(), self.0.as_ptr()) }
    }
}

/// mempool_exit(): a no-op on a pool never made.
impl Drop for Mempool {
    fn drop(&mut self) {
        unsafe { c::mempool_exit(self.0.as_ptr()) }
    }
}

/// An element of a pool, freed back to it when dropped: not zeroed.
pub struct PoolBuf<'p> {
    pool: &'p Mempool,
    p:    NonNull<u8>,
}

impl PoolBuf<'_> {
    pub fn as_ptr(&self) -> *mut u8 {
        self.p.as_ptr()
    }

    /// Its bytes, as yet anything.
    pub fn as_uninit_mut(&mut self) -> &mut [core::mem::MaybeUninit<u8>] {
        // SAFETY: the element, ours - as many bytes as the pool's elements:
        // its pool_data, as kvmalloc() gave mempool_kvmalloc() it
        unsafe {
            let size = (*self.pool.0.as_ptr()).pool_data as usize;
            core::slice::from_raw_parts_mut(self.p.as_ptr().cast(), size)
        }
    }
}

impl Drop for PoolBuf<'_> {
    fn drop(&mut self) {
        // SAFETY: alloc()'s, and ours alone
        unsafe { self.pool.free_raw(self.p) }
    }
}
