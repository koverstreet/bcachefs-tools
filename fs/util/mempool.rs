// SPDX-License-Identifier: GPL-2.0

//! The kernel's mempool - userspace's is linux/mempool.c - of elements a size
//! each, from @A: Kmalloc's or KVmalloc's, or for Raw, its own allocator's.
//! The size is its pool_data, every pool's.
//!
//! A pool of an allocator's elements is an allocator itself, of any size up
//! to its elements': Alloc, below.
//!
//! Its mempool_t is inline, C's layout. Zeroed, it isn't made; made after its
//! owner, it's a Late<Mempool>, whose lock serializes making it. It's exited
//! when it drops, a no-op on one never made. Allocating is GFP_KERNEL, as the
//! task's memalloc scope says: it waits for an element - or, in a NORECLAIM
//! scope, where it can't, fails.

use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::util::alloc::{current_gfp_context_raw, stateless, Alloc, AllocError, KVmalloc, Kmalloc};
use crate::util::ffi::Opaque;
use crate::util::late::LateInit;
use core::alloc::Layout;
use core::ffi::{c_int, c_void};
use core::marker::PhantomData;
use core::ptr::NonNull;
use pin_init::PinInit;

#[repr(transparent)]
pub struct Mempool<A>(Opaque<c::mempool_t>, PhantomData<fn() -> A>);

/// A pool of its own alloc() and free(), with(): its elements had raw -
/// alloc_raw(), free_raw() - as bounce pages are.
pub struct Raw;

// SAFETY: a mempool is every thread's, behind its own lock
unsafe impl<A> Send for Mempool<A> {}
unsafe impl<A> Sync for Mempool<A> {}
// SAFETY: zeroed is not made
unsafe impl<A> ::pin_init::Zeroable for Mempool<A> {}

unsafe impl<A> LateInit for Mempool<A> {
    fn is_made(&self) -> bool {
        unsafe { !(*self.0.as_ptr()).elements.is_null() }
    }
}

/// An allocator a pool's elements can be: C's mempool callbacks for it, and
/// what's asked of it before the pool is: as mempool_alloc() first tries for
/// an element - no reclaim or IO, no retrying, no emergency reserves: the pool
/// is the reserve.
///
/// # Safety
/// ALLOC and FREE are C's for the allocator, and first_try() gives what the
/// allocator's free() takes back.
pub unsafe trait Elements: Alloc {
    const ALLOC: unsafe extern "C" fn(c::gfp_t, *mut c_void) -> *mut c_void;
    const FREE: unsafe extern "C" fn(*mut c_void, *mut c_void);

    /// @layout's bytes, @zeroed or as they are: none, if they can't be had
    /// cheaply.
    fn first_try(layout: Layout, zeroed: bool) -> Option<NonNull<u8>>;
}

macro_rules! elements {
    ($($a:ident => $alloc:ident, $free:ident, $realloc:ident);*) => {$(
        unsafe impl Elements for $a {
            const ALLOC: unsafe extern "C" fn(c::gfp_t, *mut c_void) -> *mut c_void = c::$alloc;
            const FREE: unsafe extern "C" fn(*mut c_void, *mut c_void) = c::$free;

            fn first_try(layout: Layout, zeroed: bool) -> Option<NonNull<u8>> {
                #[cfg(kernel)]
                {
                    use kernel::bindings::{$realloc, NUMA_NO_NODE};

                    let gfp = (c::GFP_KERNEL & !(c::__GFP_DIRECT_RECLAIM | c::__GFP_IO)) |
                        c::__GFP_NORETRY | c::__GFP_NOWARN | c::__GFP_NOMEMALLOC;
                    let gfp = current_gfp_context_raw(if zeroed { gfp | c::__GFP_ZERO } else { gfp });
                    NonNull::new(unsafe {
                        $realloc(core::ptr::null(), layout.size(), layout.align() as _, gfp, NUMA_NO_NODE)
                    }.cast())
                }
                #[cfg(not(kernel))]
                {
                    let p = if zeroed { $a.alloc_zeroed(layout) } else { $a.alloc(layout) };
                    p.ok().map(NonNull::cast)
                }
            }
        }
    )*};
}

elements!(Kmalloc  => mempool_kmalloc,  mempool_kfree,  krealloc_node_align;
          KVmalloc => mempool_kvmalloc, mempool_kvfree, kvrealloc_node_align);

impl<A: Elements> Mempool<A> {
    /// @min_nr elements of @size bytes, A's. Nothing to undo if it fails.
    pub fn new(min_nr: usize, size: usize) -> impl PinInit<Self, BchError> {
        unsafe { Self::init(min_nr, A::ALLOC, A::FREE, size) }
    }
}

impl Mempool<Kmalloc> {
    /// mempool_init_kmalloc_pool().
    pub fn kmalloc(min_nr: usize, size: usize) -> impl PinInit<Self, BchError> {
        Self::new(min_nr, size)
    }
}

impl Mempool<KVmalloc> {
    pub fn kvmalloc(min_nr: usize, size: usize) -> impl PinInit<Self, BchError> {
        Self::new(min_nr, size)
    }
}

impl Mempool<Raw> {
    /// @min_nr elements of @size bytes, from @alloc and back to @free, which
    /// get @size as pool_data
    pub fn with(min_nr: usize,
                alloc: unsafe extern "C" fn(c::gfp_t, *mut c_void) -> *mut c_void,
                free: unsafe extern "C" fn(*mut c_void, *mut c_void),
                size: usize) -> impl PinInit<Self, BchError> {
        unsafe { Self::init(min_nr, alloc, free, size) }
    }
}

impl<A> Mempool<A> {
    /// # Safety
    /// @alloc and @free are A's, or a Raw pool's own.
    unsafe fn init(min_nr: usize,
                   alloc: unsafe extern "C" fn(c::gfp_t, *mut c_void) -> *mut c_void,
                   free: unsafe extern "C" fn(*mut c_void, *mut c_void),
                   size: usize) -> impl PinInit<Self, BchError> {
        unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut Self| {
                ret_to_result_void(c::mempool_init_noprof(slot.cast(), min_nr as c_int, Some(alloc),
                                                          Some(free), size as *mut c_void))
            })
        }
    }

    /// The size of its elements: the most it allocates.
    pub fn element_size(&self) -> usize {
        unsafe { (*self.0.as_ptr()).pool_data as usize }
    }

    /// An element, for C to hold: free_raw() takes it back.
    pub fn alloc_raw(&self) -> Result<NonNull<u8>, AllocError> {
        let gfp = current_gfp_context_raw(c::GFP_KERNEL);
        NonNull::new(unsafe { c::mempool_alloc_noprof(self.0.as_ptr(), gfp) }.cast()).ok_or(AllocError)
    }

    /// # Safety
    /// @p is this pool's, not used after.
    pub unsafe fn free_raw(&self, p: NonNull<u8>) {
        // SAFETY: the caller's
        unsafe { c::mempool_free(p.as_ptr().cast(), self.0.as_ptr()) }
    }
}

/// A pool as an allocator, of up to its elements' size: first what's asked,
/// if it's less than an element, from A's first_try(), then an element, the
/// worst case. free() goes by the size alloc() gave: an element back to the
/// pool, anything smaller to A.
impl<A: Elements> Mempool<A> {
    fn alloc_or_element(&self, layout: Layout, zeroed: bool) -> Result<NonNull<[u8]>, AllocError> {
        let size = self.element_size();
        if layout.size() > size || layout.align() > 8 {
            return Err(AllocError);
        }

        if layout.size() < size {
            if let Some(p) = A::first_try(layout, zeroed) {
                return Ok(NonNull::slice_from_raw_parts(p, layout.size()));
            }
        }

        let p = self.alloc_raw()?;
        if zeroed {
            unsafe { p.write_bytes(0, size) };
        }
        Ok(NonNull::slice_from_raw_parts(p, size))
    }
}

unsafe impl<A: Elements> Alloc for Mempool<A> {
    fn alloc(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        self.alloc_or_element(layout, false)
    }

    fn alloc_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        self.alloc_or_element(layout, true)
    }

    unsafe fn free(&self, p: NonNull<u8>, layout: Layout) {
        if layout.size() == self.element_size() {
            unsafe { self.free_raw(p) }
        } else {
            unsafe { stateless::<A>().free(p, layout) }
        }
    }
}

impl<A> Mempool<A> {
    /// A pool in a static - which never drops - made in place at module init.
    ///
    /// # Safety
    /// @this is unmade, zeroed, and nothing uses it till this returns.
    pub unsafe fn init_static(this: &'static Self, init: impl PinInit<Self, BchError>) -> Result<(), BchError> {
        unsafe { crate::util::alloc::init_at(init, core::ptr::from_ref(this).cast_mut()) }
    }

    /// mempool_exit(), of a pool in a static, at module exit: a no-op on one
    /// never made.
    ///
    /// # Safety
    /// Nothing allocates from it, or frees to it, after.
    pub unsafe fn exit(&self) {
        unsafe { c::mempool_exit(self.0.as_ptr()) }
    }
}

/// mempool_exit(): a no-op on a pool never made.
impl<A> Drop for Mempool<A> {
    fn drop(&mut self) {
        unsafe { self.exit() }
    }
}
