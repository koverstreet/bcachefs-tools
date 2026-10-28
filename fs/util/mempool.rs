// SPDX-License-Identifier: GPL-2.0

//! The kernel's mempool - userspace's is linux/mempool.c - of kvmalloc()'d
//! elements: mempool_init_kvmalloc_pool(), util.c's element allocator.
//!
//! A Mempool is set up once, when it's first wanted, and used from then on
//! without a lock. Its mempool_t is inline, as C's, and set up in place by
//! whoever claims it first - @state 0 to 1 - so setting it up is safe from
//! anywhere: a racing caller waits for the claimant's, or claims it again if
//! that failed. Zeroed, it isn't set up. It's exited when it drops, a no-op on
//! one never set up. Allocating waits for an element: GFP_KERNEL, the
//! caller's memalloc scope saying the rest.

use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::util::ffi::Opaque;
use crate::util::kernel::cond_resched;
use core::ffi::{c_int, c_void};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU8, Ordering::{Acquire, Release}};

const UNSET: u8 = 0;
const SETTING_UP: u8 = 1;
const READY: u8 = 2;

/// # Invariants
/// @state only goes UNSET -> SETTING_UP -> READY, or back to UNSET if setting
/// up failed: @pool is set up only at READY, and while SETTING_UP is its one
/// claimant's alone.
#[repr(C)]
pub struct Mempool {
    pool:  Opaque<c::mempool_t>,
    state: AtomicU8,
}

// SAFETY: a mempool is every thread's, behind its own lock - and set up by
// one, as above
unsafe impl Send for Mempool {}
unsafe impl Sync for Mempool {}
// SAFETY: zeroed is UNSET
unsafe impl ::pin_init::Zeroable for Mempool {}

impl Mempool {
    pub fn initialized(&self) -> bool {
        self.state.load(Acquire) == READY
    }

    /// Set up with @min_nr elements of @size bytes, if it isn't.
    pub fn init_kvmalloc(&self, min_nr: usize, size: usize) -> Result<(), BchError> {
        loop {
            match self.state.compare_exchange(UNSET, SETTING_UP, Acquire, Acquire) {
                Ok(_) => {
                    // SAFETY: claimed; mempool_init() fails having freed what it made
                    let ret = ret_to_result_void(unsafe {
                        c::mempool_init_noprof(self.pool.as_ptr(), min_nr as c_int, Some(c::mempool_kvmalloc),
                                               Some(c::mempool_kvfree), size as *mut c_void)
                    });
                    self.state.store(if ret.is_ok() { READY } else { UNSET }, Release);
                    return ret;
                }
                Err(READY) => return Ok(()),
                // another's setting it up
                Err(_) => cond_resched(),
            }
        }
    }

    /// An element, freed to the pool on drop.
    pub fn alloc(&self) -> PoolBuf<'_> {
        PoolBuf { pool: self, p: self.alloc_raw() }
    }

    /// alloc(), for C to hold: free_raw() takes it back.
    pub fn alloc_raw(&self) -> NonNull<u8> {
        assert!(self.initialized(), "allocating from a mempool not set up");
        // SAFETY: READY
        NonNull::new(unsafe { c::mempool_alloc_noprof(self.pool.as_ptr(), c::GFP_KERNEL) }.cast())
            .expect("mempool_alloc() waits for an element")
    }

    /// # Safety
    /// @p is this pool's, not used after.
    pub unsafe fn free_raw(&self, p: NonNull<u8>) {
        // SAFETY: the caller's
        unsafe { c::mempool_free(p.as_ptr().cast(), self.pool.as_ptr()) }
    }
}

/// mempool_exit(): a no-op on a pool never set up.
impl Drop for Mempool {
    fn drop(&mut self) {
        // SAFETY: &mut: no one's - nor being set up
        unsafe { c::mempool_exit(self.pool.as_ptr()) }
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
        // its pool_data, as init_kvmalloc() gave mempool_kvmalloc() it
        unsafe {
            let size = (*self.pool.pool.as_ptr()).pool_data as usize;
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
