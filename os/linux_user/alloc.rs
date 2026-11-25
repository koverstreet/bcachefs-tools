// SPDX-License-Identifier: GPL-2.0

//! Allocation, for userspace, for util/alloc.rs: Kmalloc, Vmalloc and KVmalloc
//! over std's allocator, which takes no flags and no memalloc scope. What's
//! freed a grace period on, by an rcu_head of no size it knows, is malloc()'s.

#[path = "../alloc.rs"]
mod common;
pub use common::*;

pub use std::sync::Arc;

use crate::c;
use crate::util::alloc::{kv_layout, Alloc, AllocError, AllocGfp, Allocator, KVmalloc, Kmalloc, Vmalloc};
use crate::util::rcu::FreeAnytime;
use core::alloc::Layout;
use core::ffi::c_void;
use core::ptr::NonNull;

/// In userspace, a Box is a KBox.
pub type KBox<T> = std::boxed::Box<T>;
/// And a KVBox.
pub type KVBox<T> = std::boxed::Box<T>;

impl Kmalloc {
    /// The kernel's, PAGE_SIZE << MAX_PAGE_ORDER: what's too big for a
    /// kmalloc() there is here.
    pub const MAX_SIZE: usize = super::kernel::PAGE_SIZE << 10;
}

macro_rules! allocs {
    ($($a:ident),*) => {$(
        unsafe impl Allocator for $a {
            fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
                self.allocate_gfp(layout, Flags::empty())
            }

            fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
                self.allocate_zeroed_gfp(layout, Flags::empty())
            }

            unsafe fn deallocate(&self, p: NonNull<u8>, layout: Layout) {
                if layout.size() != 0 {
                    unsafe { std::alloc::dealloc(p.as_ptr(), layout) };
                }
            }

            unsafe fn grow(&self, p: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
                unsafe { self.grow_gfp(p, old, new, Flags::empty()) }
            }
        }

        unsafe impl Alloc for $a {
            const KVFREE: bool = true;
        }

        unsafe impl AllocGfp for $a {
            fn allocate_gfp(&self, layout: Layout, gfp: Flags) -> Result<NonNull<[u8]>, AllocError> {
                if layout.size() == 0 {
                    return Ok(dangling(layout));
                }
                let p = if gfp.contains(flags::ZERO) {
                    unsafe { std::alloc::alloc_zeroed(layout) }
                } else {
                    unsafe { std::alloc::alloc(layout) }
                };
                Ok(NonNull::slice_from_raw_parts(allocated(p, layout, gfp)?, layout.size()))
            }

            unsafe fn grow_gfp(&self, p: NonNull<u8>, old: Layout, new: Layout, gfp: Flags)
                -> Result<NonNull<[u8]>, AllocError>
            {
                // realloc() keeps the old alignment, and wants a block to keep
                if old.size() == 0 || old.align() != new.align() {
                    let q = self.allocate_gfp(new, gfp)?;
                    unsafe {
                        q.cast::<u8>().copy_from_nonoverlapping(p, old.size());
                        self.deallocate(p, old);
                    }
                    return Ok(q);
                }
                let q = allocated(unsafe { std::alloc::realloc(p.as_ptr(), old, new.size()) }, new, gfp)?;
                if gfp.contains(flags::ZERO) {
                    unsafe { q.add(old.size()).write_bytes(0, new.size() - old.size()) };
                }
                Ok(NonNull::slice_from_raw_parts(q, new.size()))
            }
        }
    )*};
}

allocs!(Kmalloc, KVmalloc, Vmalloc);

/// A zero-sized allocation: none, aligned.
fn dangling(layout: Layout) -> NonNull<[u8]> {
    let p = NonNull::new(core::ptr::without_provenance_mut(layout.align())).expect("an alignment is nonzero");
    NonNull::slice_from_raw_parts(p, 0)
}

/// What an allocation of @layout gave: null is an AllocError - or for NOFAIL,
/// std's allocation failure, which aborts.
fn allocated(p: *mut u8, layout: Layout, gfp: Flags) -> Result<NonNull<u8>, AllocError> {
    match NonNull::new(p) {
        Some(p) => Ok(p),
        None if gfp.contains(flags::NOFAIL) => std::alloc::handle_alloc_error(layout),
        None => Err(AllocError),
    }
}

/// @gfp: there's no memalloc scope to apply.
pub fn current_gfp_context(gfp: Flags) -> Flags {
    gfp
}

/// A plain allocation's gfp_t, for C that takes one: none - the shim's C,
/// percpu_ref_init(), takes one and ignores it.
pub fn current_gfp() -> c::gfp_t {
    0
}

/// The last ref's put frees it at once: what RCU readers may reach, which
/// isn't FreeAnytime, can't be in one.
pub fn arc_new<T: Send + Sync + FreeAnytime>(v: T) -> Result<Arc<T>, AllocError> {
    Ok(Arc::new(v))
}

/// @b's value, out of its box: *b, for a std Box.
pub fn kbox_into_inner<T>(b: KBox<T>) -> T {
    *b
}

/// kmalloc(), as the shim's slab.h had it: aligned to the largest power of two
/// in @size, up to a page; tried ten times, the shrinkers run between; zeroed
/// with ZERO. Null if it can't - or for NOFAIL, an abort. free() takes it back.
fn kmalloc_noprof(size: usize, gfp: Flags) -> *mut c_void {
    for _ in 0..10 {
        let p = if size != 0 {
            let align = (1usize << size.ilog2()).min(super::kernel::PAGE_SIZE).max(size_of::<*mut c_void>());
            let mut p = core::ptr::null_mut();
            if unsafe { libc::posix_memalign(&mut p, align, size) } != 0 {
                p = core::ptr::null_mut();
            }
            p
        } else {
            unsafe { libc::malloc(0) }
        };

        if !p.is_null() {
            if gfp.contains(flags::ZERO) {
                unsafe { p.cast::<u8>().write_bytes(0, size) };
            }
            return p;
        }

        super::shrinker::run_shrinkers(gfp, true);
    }
    if gfp.contains(flags::NOFAIL) {
        std::alloc::handle_alloc_error(Layout::from_size_align(size, 1).expect("an allocation's"));
    }
    core::ptr::null_mut()
}

/// @size bytes kvzalloc()'d, NOFAIL: kvfree_rcu_head() or
/// kfree_rcu_mightsleep() takes it back.
pub fn kvzalloc_nofail(size: usize) -> NonNull<u8> {
    NonNull::new(kmalloc_noprof(size, flags::ZERO | flags::NOFAIL).cast()).expect("a NOFAIL allocation failed")
}

/// A zeroed KBox that can't fail to allocate: std's allocator aborts rather
/// than fail.
pub fn kbox_zeroed_nofail<T: ::pin_init::Zeroable + FreeAnytime>() -> KBox<T> {
    // kmalloc()'s alignment, at least
    const { assert!(size_of::<T>() != 0 && align_of::<T>() <= 8) };

    let layout = Layout::new::<T>();
    // SAFETY: not zero sized
    let p = unsafe { std::alloc::alloc_zeroed(layout) }.cast::<T>();
    if p.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    // SAFETY: the global allocator's, of a T's layout; zeroed is a valid T
    unsafe { KBox::from_raw(p) }
}

/// @layout's worth of memory, zeroed, as a KBox allocates: for a KBox of an
/// unsized type it can't make itself - a struct ending in a slice - by
/// KBox::from_raw(). Or kfree(), if it's never made.
pub fn kzalloc(layout: Layout) -> Result<NonNull<u8>, AllocError> {
    // SAFETY: not zero sized, by the caller's use
    NonNull::new(unsafe { std::alloc::alloc_zeroed(layout) }).ok_or(AllocError)
}

/// # Safety
/// @p is kzalloc()'s, of @layout, and nothing uses it after.
pub unsafe fn kfree(p: NonNull<u8>, layout: Layout) {
    // SAFETY: the caller's
    unsafe { std::alloc::dealloc(p.as_ptr(), layout) };
}

/// kvzalloc(), with @gfp, which takes nothing here. kvfree_sized() takes it
/// back.
pub fn kvzalloc_gfp(size: usize, _gfp: Flags) -> Result<NonNull<u8>, AllocError> {
    NonNull::new(unsafe { std::alloc::alloc_zeroed(kv_layout(size)?) }).ok_or(AllocError)
}

/// @p, @old bytes of kvmalloc()'d memory - or none, null - as @new bytes,
/// what was there kept and the rest zeroed: kvrealloc(), __GFP_ZERO.
/// Aligned as kmalloc() aligns @new. kvfree_sized() takes it back.
pub fn kvrealloc_zeroed(p: *mut u8, old: usize, new: usize) -> Result<NonNull<u8>, AllocError> {
    // A new allocation and a copy: realloc() keeps the old one's alignment
    // SAFETY: @p is std::alloc's, of @old bytes, or null
    unsafe {
        let q = NonNull::new(std::alloc::alloc_zeroed(kv_layout(new)?)).ok_or(AllocError)?;
        if let Some(p) = NonNull::new(p) {
            q.as_ptr().copy_from_nonoverlapping(p.as_ptr(), old.min(new));
            std::alloc::dealloc(p.as_ptr(), kv_layout(old)?);
        }
        Ok(q)
    }
}

/// @p, @old bytes of kvmalloc()'d memory - or none, null - as @new bytes,
/// what was there kept: kvrealloc(), with @gfp. Aligned as kmalloc() aligns
/// @new; kvfree_sized() takes it back.
pub fn kvrealloc(p: *mut u8, old: usize, new: usize, gfp: Flags) -> Result<NonNull<u8>, AllocError> {
    // A new allocation and a copy: realloc() keeps the old one's alignment
    // SAFETY: @p is std::alloc's, of @old bytes, or null
    unsafe {
        let layout = kv_layout(new)?;
        let q = allocated(std::alloc::alloc(layout), layout, gfp)?;
        if let Some(p) = NonNull::new(p) {
            q.as_ptr().copy_from_nonoverlapping(p.as_ptr(), old.min(new));
            std::alloc::dealloc(p.as_ptr(), kv_layout(old)?);
        }
        Ok(q)
    }
}

/// # Safety
/// @p is kvrealloc_zeroed()'s or kvrealloc()'s, of @size bytes, and nothing
/// uses it after.
pub unsafe fn kvfree_sized(p: NonNull<u8>, size: usize) {
    let layout = kv_layout(size).expect("its allocation's");
    // SAFETY: the caller's
    unsafe { std::alloc::dealloc(p.as_ptr(), layout) };
}

/// C's kvfree_rcu(): @p freed a grace period on - by a callback of its own,
/// as rcu_free_ptr() frees it, not by @head.
///
/// # Safety
/// Only RCU readers use @p after.
pub unsafe fn kvfree_rcu_head(p: *mut c_void, _head: *mut c::rcu_head) {
    unsafe { rcu_free_ptr(p) }
}

/// C's kfree_rcu_mightsleep(): @p freed a grace period on.
///
/// # Safety
/// Only RCU readers use @p after.
pub unsafe fn kfree_rcu_mightsleep(p: *mut c_void) {
    unsafe { rcu_free_ptr(p) }
}

/*
 * The rcu_head embedded in the object can't be used directly: call_rcu() hands
 * the callback &obj->rcu, and free() needs the base of the allocation, which
 * isn't recoverable from that without knowing the field's offset. So carry the
 * pointer in a wrapper.
 *
 * If the wrapper can't be allocated, block for a grace period rather than free
 * under readers - slow, but the alternative is the bug this replaced.
 */
#[repr(C)]
struct RcuFreeWrapper {
    rcu: c::rcu_head,
    p:   *mut c_void,
}

unsafe extern "C" fn rcu_free_wrapper_cb(head: *mut c::rcu_head) {
    let w = head.cast::<RcuFreeWrapper>();
    unsafe {
        libc::free((*w).p);
        libc::free(w.cast());
    }
}

/// @p, malloc()'s, freed a grace period on.
///
/// # Safety
/// Only RCU readers use @p after.
pub(super) unsafe fn rcu_free_ptr(p: *mut c_void) {
    if p.is_null() {
        return;
    }

    let w = unsafe { libc::malloc(size_of::<RcuFreeWrapper>()) }.cast::<RcuFreeWrapper>();
    if !w.is_null() {
        unsafe {
            (&raw mut (*w).p).write(p);
            super::rcu::Flavor::RCU.call_raw(&raw mut (*w).rcu, rcu_free_wrapper_cb);
        }
    } else {
        super::rcu::synchronize();
        unsafe { libc::free(p) };
    }
}

/// @size bytes, with @gfp, for a bio's data: kmalloc()'s, here.
pub fn bio_pages_alloc(size: usize, gfp: Flags) -> *mut c_void {
    kmalloc_noprof(size, gfp)
}

/// # Safety
/// @p is bio_pages_alloc()'s, and nothing uses it after.
pub unsafe fn bio_pages_free(p: *mut c_void, _size: usize) {
    unsafe { libc::free(p) }
}
