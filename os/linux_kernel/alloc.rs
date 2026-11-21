// SPDX-License-Identifier: GPL-2.0

//! The kernel's allocation, for util/alloc.rs: Kmalloc, Vmalloc and KVmalloc
//! are core's Allocator over C's krealloc(), vrealloc() and kvrealloc(), with
//! our Flags as the task's memalloc scope says - and the kernel's Rust
//! Allocator, for its Box and Arc, the same. What the kernel's Rust doesn't
//! have - a kfree() of no size - is through shim.c.

#[path = "../alloc.rs"]
mod common;
pub use common::*;

pub use kernel::sync::Arc;

use crate::c;
use crate::util::alloc::{kv_layout, Alloc, AllocError, AllocGfp, Allocator, KVmalloc, Kmalloc, Vmalloc};
use crate::util::rcu::{DropAnytime, DropAtomic, FreeAnytime, NothingToDetach};
use core::alloc::Layout;
use core::ffi::{c_int, c_void};
use core::ptr::NonNull;
use kernel::alloc::NumaNode;
use kernel::bindings::{krealloc_node_align, kvrealloc_node_align};

pub type KBox<T> = kernel::alloc::Box<T, Kmalloc>;
pub type KVBox<T> = kernel::alloc::Box<T, KVmalloc>;

impl Kmalloc {
    pub const MAX_SIZE: usize = super::kernel::PAGE_SIZE << c::MAX_PAGE_ORDER;
}

/// What a plain allocation may do that NOWAIT, ATOMIC and NOIO take away.
const GFP_DIRECT: c::gfp_t = c::__GFP_DIRECT_RECLAIM | c::__GFP_IO | c::__GFP_FS;

impl Flags {
    /// The kernel's gfp_t of them: GFP_KERNEL, GFP_NOWAIT, GFP_ATOMIC or
    /// GFP_NOIO, and the modifiers.
    pub const fn raw(self) -> c::gfp_t {
        const GFP: [(Flags, c::gfp_t); 8] = [
            (flags::ZERO,        c::__GFP_ZERO),
            (flags::NOWARN,      c::__GFP_NOWARN),
            (flags::NORETRY,     c::__GFP_NORETRY),
            (flags::NO_RESERVES, c::__GFP_NOMEMALLOC),
            (flags::ACCOUNT,     c::__GFP_ACCOUNT),
            (flags::RECLAIMABLE, c::__GFP_RECLAIMABLE),
            (flags::SKIP_ZERO,   c::__GFP_SKIP_ZERO),
            (flags::NOFAIL,      c::__GFP_NOFAIL),
        ];

        let mut gfp = if self.contains(flags::ATOMIC) {
            c::GFP_KERNEL & !GFP_DIRECT | c::__GFP_HIGH
        } else if self.contains(flags::NOWAIT) {
            c::GFP_KERNEL & !GFP_DIRECT | c::__GFP_NOWARN
        } else if self.contains(flags::NOIO) {
            c::GFP_KERNEL & !(c::__GFP_IO | c::__GFP_FS)
        } else {
            c::GFP_KERNEL
        };
        let mut i = 0;
        while i < GFP.len() {
            if self.contains(GFP[i].0) {
                gfp |= GFP[i].1;
            }
            i += 1;
        }
        gfp
    }
}

// The kernel's, under its Box: with the flags the task's memalloc scope says,
// so a KBox honours NORECLAIM and NOWARN, which the page allocator doesn't
// know.
macro_rules! allocators {
    ($($a:ident),*) => {$(
        unsafe impl kernel::alloc::Allocator for $a {
            const MIN_ALIGN: usize = <kernel::alloc::allocator::$a as kernel::alloc::Allocator>::MIN_ALIGN;

            unsafe fn realloc(ptr: Option<NonNull<u8>>, layout: Layout, old_layout: Layout,
                              flags: kernel::alloc::Flags, nid: NumaNode)
                -> Result<NonNull<[u8]>, kernel::alloc::AllocError>
            {
                unsafe {
                    <kernel::alloc::allocator::$a as kernel::alloc::Allocator>::realloc(
                        ptr, layout, old_layout, kernel_gfp_context(flags), nid)
                }
            }

            unsafe fn free(ptr: NonNull<u8>, layout: Layout) {
                unsafe { <kernel::alloc::allocator::$a as kernel::alloc::Allocator>::free(ptr, layout) }
            }
        }
    )*};
}

allocators!(Kmalloc, KVmalloc);

macro_rules! allocs {
    ($($a:ident => $realloc:expr, $layout:expr),*) => {$(
        unsafe impl Allocator for $a {
            fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
                self.allocate_gfp(layout, Flags::empty())
            }

            fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
                self.allocate_zeroed_gfp(layout, Flags::empty())
            }

            unsafe fn deallocate(&self, p: NonNull<u8>, layout: Layout) {
                let _ = unsafe { $a::c_realloc(Some(p), Layout::new::<()>(), layout, Flags::empty()) };
            }

            unsafe fn grow(&self, p: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
                unsafe { $a::c_realloc(Some(p), new, old, Flags::empty()) }
            }

            unsafe fn grow_zeroed(&self, p: NonNull<u8>, old: Layout, new: Layout)
                -> Result<NonNull<[u8]>, AllocError>
            {
                unsafe { $a::c_realloc(Some(p), new, old, flags::ZERO) }
            }

            unsafe fn shrink(&self, p: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
                unsafe { $a::c_realloc(Some(p), new, old, Flags::empty()) }
            }
        }

        unsafe impl Alloc for $a {
            const KVFREE: bool = true;
        }

        unsafe impl AllocGfp for $a {
            fn allocate_gfp(&self, layout: Layout, gfp: Flags) -> Result<NonNull<[u8]>, AllocError> {
                unsafe { $a::c_realloc(None, layout, layout, gfp) }
            }

            unsafe fn grow_gfp(&self, p: NonNull<u8>, old: Layout, new: Layout, gfp: Flags)
                -> Result<NonNull<[u8]>, AllocError>
            {
                unsafe { $a::c_realloc(Some(p), new, old, gfp) }
            }
        }

        impl $a {
            /// C's krealloc(), vrealloc() or kvrealloc(), as the kernel's Rust
            /// allocator calls them, with @gfp as the task's memalloc scope
            /// says: @p - none for a new allocation - as @layout.
            ///
            /// # Safety
            /// @p is this allocator's, of @old.
            unsafe fn c_realloc(p: Option<NonNull<u8>>, layout: Layout, old: Layout, gfp: Flags)
                -> Result<NonNull<[u8]>, AllocError>
            {
                use kernel::bindings::NUMA_NO_NODE;

                let layout: Layout = $layout(layout);
                // a zero-sized allocation is dangling: nothing for C to free
                let p = p.filter(|_| old.size() != 0).map_or(core::ptr::null(), |p| p.as_ptr().cast_const().cast());
                ebug_assert!(!gfp.contains(flags::NOFAIL) || !memalloc_scope().0,
                             "a NOFAIL allocation, in a NORECLAIM scope");
                let gfp = current_gfp_context_raw(gfp.raw());
                let q = ($realloc)(p, old.size(), layout.size(), layout.align(), gfp, NUMA_NO_NODE);
                let q = if layout.size() == 0 {
                    core::ptr::without_provenance_mut(layout.align())
                } else {
                    q.cast::<u8>()
                };
                Ok(NonNull::slice_from_raw_parts(NonNull::new(q).ok_or(AllocError)?, layout.size()))
            }
        }
    )*};
}

// kmalloc() sizes a size padded to its alignment, as the kernel's Rust has it.
allocs!(Kmalloc  => |p, _, size, align, gfp, nid| unsafe { krealloc_node_align(p, size, align, gfp, nid) },
                    |l: Layout| l.pad_to_align(),
        Vmalloc  => |p, old, size, align, gfp, nid| unsafe { vrealloc(p, old, size, align, gfp, nid) },
                    |l: Layout| l,
        KVmalloc => |p, _, size, align, gfp, nid| unsafe { kvrealloc_node_align(p, size, align, gfp, nid) },
                    |l: Layout| l.pad_to_align());

/// C's vrealloc(): @p, of @old bytes, as @size - freed if 0.
#[cfg(linux_ge = "7.0")]
unsafe fn vrealloc(p: *const c_void, _old: usize, size: usize, align: usize, gfp: c::gfp_t, nid: c_int)
    -> *mut c_void
{
    unsafe { kernel::bindings::vrealloc_node_align(p, size, align, gfp, nid) }
}

/// Before 7.0 vrealloc() isn't exported, nor __vmalloc_node() but to the
/// vmalloc test module - __vmalloc() is: page aligned, on any node, and a
/// resize is a copy into a new allocation.
#[cfg(not(linux_ge = "7.0"))]
unsafe fn vrealloc(p: *const c_void, old: usize, size: usize, align: usize, gfp: c::gfp_t, _nid: c_int)
    -> *mut c_void
{
    if align > super::kernel::PAGE_SIZE {
        return core::ptr::null_mut();
    }
    unsafe {
        if size == 0 {
            c::vfree(p);
            return core::ptr::null_mut();
        }
        let q = c::__vmalloc_noprof(size as _, gfp);
        if !q.is_null() && !p.is_null() {
            core::ptr::copy_nonoverlapping(p.cast::<u8>(), q.cast::<u8>(), old.min(size));
            c::vfree(p);
        }
        q
    }
}

// The kernel's Rust Box and Arc fail with an AllocError of their own.
impl From<kernel::alloc::AllocError> for crate::errcode::BchError {
    fn from(_: kernel::alloc::AllocError) -> Self {
        AllocError.into()
    }
}

// The kernel's Box and Vec free as their T would.
macro_rules! held_pointers {
    ($($trait:ident),*) => {$(
        unsafe impl<T: ?Sized + $trait, A: kernel::alloc::Allocator> $trait for kernel::alloc::Box<T, A> {}
        unsafe impl<T: $trait, A: kernel::alloc::Allocator> $trait for kernel::alloc::kvec::Vec<T, A> {}
    )*};
}

held_pointers!(FreeAnytime, DropAnytime, DropAtomic, NothingToDetach);

/// What the task's memalloc scope takes from an allocation's flags, and what
/// it adds - the scopes the page allocator doesn't apply itself, as it does
/// NOIO and NOFS: NORECLAIM takes direct reclaim, and the IO and FS that mean
/// nothing without it; NOWARN adds NOWARN.
fn memalloc_scope() -> (bool, bool) {
    use crate::locking::{PF_MEMALLOC_NORECLAIM, PF_MEMALLOC_NOWARN};

    let pf = crate::os::sched::memalloc_flags();
    (pf & PF_MEMALLOC_NORECLAIM != 0, pf & PF_MEMALLOC_NOWARN != 0)
}

/// @gfp as the task's memalloc scope says: NORECLAIM makes it NOWAIT, if it
/// isn't ATOMIC; NOWARN adds NOWARN.
pub fn current_gfp_context(gfp: Flags) -> Flags {
    let (noreclaim, nowarn) = memalloc_scope();
    let gfp = if noreclaim && !gfp.contains(flags::ATOMIC) { gfp | flags::NOWAIT } else { gfp };
    if nowarn { gfp | flags::NOWARN } else { gfp }
}

/// current_gfp_context(), for a gfp_t of the kernel's own - a mapping's, which
/// the VFS hands us, or ours: NORECLAIM takes direct reclaim, and the IO and
/// FS that mean nothing without it.
pub fn current_gfp_context_raw(gfp: c::gfp_t) -> c::gfp_t {
    let (noreclaim, nowarn) = memalloc_scope();
    let gfp = if noreclaim { gfp & !GFP_DIRECT } else { gfp };
    if nowarn { gfp | c::__GFP_NOWARN } else { gfp }
}

/// A plain allocation's gfp_t, as the task's memalloc scope says: for C that
/// takes one.
pub fn current_gfp() -> c::gfp_t {
    current_gfp_context_raw(c::GFP_KERNEL)
}

/// current_gfp_context(), for the kernel's Rust Flags - its Box's and Arc's -
/// which name no direct reclaim, IO or FS: GFP_KERNEL less GFP_NOWAIT is
/// those.
fn kernel_gfp_context(f: kernel::alloc::Flags) -> kernel::alloc::Flags {
    use kernel::alloc::flags::{GFP_KERNEL, GFP_NOWAIT, __GFP_NOWARN};

    let (noreclaim, nowarn) = memalloc_scope();
    let f = if noreclaim { f & !(GFP_KERNEL & !GFP_NOWAIT) } else { f };
    if nowarn { f | __GFP_NOWARN } else { f }
}

/// The last ref's put frees it at once: what RCU readers may reach, which
/// isn't FreeAnytime, can't be in one.
pub fn arc_new<T: Send + Sync + FreeAnytime>(v: T) -> Result<Arc<T>, AllocError> {
    Arc::new(v, kernel_gfp_context(kernel::alloc::flags::GFP_KERNEL)).map_err(|_| AllocError)
}

/// @b's value, out of its box: the kernel's Box has into_inner().
pub fn kbox_into_inner<T>(b: KBox<T>) -> T {
    KBox::into_inner(b)
}

/// @size bytes kzalloc()'d, NOFAIL: never null. kfree_unsized() takes it back.
pub fn kzalloc_nofail(size: usize) -> *mut c_void {
    kzalloc_gfp(size, flags::NOFAIL).expect("a NOFAIL allocation failed").as_ptr().cast()
}

/// @size bytes kvzalloc()'d, NOFAIL. kvfree_unsized() takes it back, or
/// kvfree_rcu_head().
pub fn kvzalloc_nofail(size: usize) -> NonNull<u8> {
    kvzalloc_gfp(size, flags::NOFAIL).expect("a NOFAIL allocation failed")
}

/// A zeroed KBox that can't fail to allocate: Kmalloc's, NOFAIL - which the
/// kernel's Rust Box can't ask for.
pub fn kbox_zeroed_nofail<T: ::pin_init::Zeroable + FreeAnytime>() -> KBox<T> {
    let p = Kmalloc.allocate_zeroed_gfp(Layout::new::<T>(), flags::NOFAIL)
        .expect("a NOFAIL allocation failed");
    unsafe { KBox::from_raw(p.as_ptr().cast::<T>()) }
}

/// @layout's worth of memory, zeroed, as a KBox allocates: for a KBox of an
/// unsized type it can't make itself - a struct ending in a slice - by
/// KBox::from_raw(). Or kfree(), if it's never made.
pub fn kzalloc(layout: Layout) -> Result<NonNull<u8>, AllocError> {
    Ok(Kmalloc.allocate_zeroed(layout)?.cast())
}

/// # Safety
/// @p is kzalloc()'s, of @layout, and nothing uses it after.
pub unsafe fn kfree(p: NonNull<u8>, layout: Layout) {
    // SAFETY: the caller's
    unsafe { Kmalloc.deallocate(p, layout) };
}

/// @p - kmalloc()'d, or null - as @new bytes, what was there kept: krealloc(),
/// for a buffer C owns and kfree()s. Of no size the caller knows, as C's
/// aren't. GFP_KERNEL, the caller's memalloc scope saying the rest.
///
/// # Safety
/// @p is null, or kmalloc()'s and not used after unless this fails.
pub unsafe fn krealloc(p: *mut u8, new: usize) -> Result<NonNull<u8>, AllocError> {
    // Kmalloc::realloc() wants the old allocation's layout, which nobody has
    use kernel::bindings::{krealloc_node_align, NUMA_NO_NODE};

    // SAFETY: the caller's
    let gfp = current_gfp();
    NonNull::new(unsafe { krealloc_node_align(p.cast(), new, 1, gfp, NUMA_NO_NODE) }.cast()).ok_or(AllocError)
}

/// krealloc()'s, kvmalloc()'d: what C's kvfree() takes back.
///
/// # Safety
/// @p is null, or kvmalloc()'s and not used after unless this fails.
pub unsafe fn kvrealloc_unsized(p: *mut u8, new: usize) -> Result<NonNull<u8>, AllocError> {
    use kernel::bindings::{kvrealloc_node_align, NUMA_NO_NODE};

    let gfp = current_gfp();
    NonNull::new(unsafe { kvrealloc_node_align(p.cast(), new, 1, gfp, NUMA_NO_NODE) }.cast()).ok_or(AllocError)
}

/// @size bytes kmalloc()'d, uninitialized: for a buffer of no size anyone
/// keeps - C's kfree(), or kfree_unsized(), takes it back.
pub fn kmalloc(size: usize) -> Result<NonNull<u8>, AllocError> {
    let layout = Layout::from_size_align(size, 1).map_err(|_| AllocError)?;
    Ok(Kmalloc.allocate(layout)?.cast())
}

/// kzalloc(), with @gfp. kfree_unsized() takes it back.
pub fn kzalloc_gfp(size: usize, gfp: Flags) -> Result<NonNull<u8>, AllocError> {
    let layout = Layout::from_size_align(size, 1).map_err(|_| AllocError)?;
    Ok(Kmalloc.allocate_zeroed_gfp(layout, gfp)?.cast())
}

/// kvzalloc(), with @gfp. kvfree_sized() takes it back.
pub fn kvzalloc_gfp(size: usize, gfp: Flags) -> Result<NonNull<u8>, AllocError> {
    Ok(KVmalloc.allocate_zeroed_gfp(kv_layout(size)?, gfp)?.cast())
}

/// @p, @old bytes of kvmalloc()'d memory - or none, null - as @new bytes,
/// what was there kept and the rest zeroed: kvrealloc(), __GFP_ZERO.
/// Aligned as kmalloc() aligns @new. kvfree_sized() takes it back.
pub fn kvrealloc_zeroed(p: *mut u8, old: usize, new: usize) -> Result<NonNull<u8>, AllocError> {
    // SAFETY: @p is KVmalloc's, of @old bytes, or null
    Ok(unsafe { KVmalloc::c_realloc(NonNull::new(p), kv_layout(new)?, kv_layout(old)?, flags::ZERO) }?.cast())
}

/// @p, @old bytes of kvmalloc()'d memory - or none, null - as @new bytes,
/// what was there kept: kvrealloc(), with @gfp. Aligned as kmalloc() aligns
/// @new; kvfree_sized() takes it back.
pub fn kvrealloc(p: *mut u8, old: usize, new: usize, gfp: Flags) -> Result<NonNull<u8>, AllocError> {
    // SAFETY: @p is KVmalloc's, of @old bytes, or null
    Ok(unsafe { KVmalloc::c_realloc(NonNull::new(p), kv_layout(new)?, kv_layout(old)?, gfp) }?.cast())
}

/// # Safety
/// @p is kvrealloc_zeroed()'s or kvrealloc()'s, of @size bytes, and nothing
/// uses it after.
pub unsafe fn kvfree_sized(p: NonNull<u8>, size: usize) {
    let layout = kv_layout(size).expect("its allocation's");
    // SAFETY: the caller's
    unsafe { KVmalloc.deallocate(p, layout) };
}

/// C's kfree(): @p, kmalloc()'d, of no size the caller knows - or null.
///
/// # Safety
/// Nothing uses @p after.
pub unsafe fn kfree_unsized(p: *const c_void) {
    unsafe { c::rust_kfree(p) }
}

/// C's kvfree(): @p, kvmalloc()'d, of no size the caller knows - or null.
///
/// # Safety
/// Nothing uses @p after.
pub unsafe fn kvfree_unsized(p: *mut c_void) {
    unsafe { c::rust_kvfree(p) }
}

/// C's kvfree_rcu(): @p, kvmalloc()'d, freed a grace period on, by @head in
/// it.
///
/// # Safety
/// @head is in @p, and only RCU readers use @p after.
pub unsafe fn kvfree_rcu_head(p: *mut c_void, head: *mut c::rcu_head) {
    unsafe { c::rust_kvfree_rcu(p, head) }
}

/// C's kfree_rcu_mightsleep(): @p, kmalloc()'d, freed a grace period on -
/// which it may wait out, having nowhere to queue it.
///
/// # Safety
/// Only RCU readers use @p after.
pub unsafe fn kfree_rcu_mightsleep(p: *mut c_void) {
    unsafe { c::rust_kfree_rcu_mightsleep(p) }
}

/// @size bytes of pages, with @gfp and the task's memalloc scope, for a bio's
/// data: page aligned, as we don't know the device's DMA alignment.
pub fn bio_pages_alloc(size: usize, gfp: Flags) -> *mut c_void {
    unsafe { c::rust_bio_pages_alloc(size, current_gfp_context_raw(gfp.raw())) }
}

/// # Safety
/// @p is bio_pages_alloc()'s, of @size bytes, and nothing uses it after.
pub unsafe fn bio_pages_free(p: *mut c_void, size: usize) {
    unsafe { c::rust_bio_pages_free(p, size) }
}
