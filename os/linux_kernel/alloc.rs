// SPDX-License-Identifier: GPL-2.0

//! The kernel's allocation, for util/alloc.rs: kernel::alloc over Kmalloc and
//! KVmalloc, which apply the task's memalloc scope; and what C has that the
//! kernel's Rust doesn't - __GFP_NOFAIL, a kfree() of no size - through
//! shim.c.

pub use kernel::alloc::{flags, AllocError, Flags};
pub use kernel::sync::Arc;

use crate::c;
use crate::util::alloc::{kv_layout, Alloc, AllocGfp, KVmalloc, Kmalloc};
use crate::util::rcu::{DropAnytime, DropAtomic, FreeAnytime, NothingToDetach};
use core::alloc::Layout;
use core::ffi::c_void;
use core::ptr::NonNull;
use kernel::alloc::{Allocator, NumaNode};

pub type KBox<T> = kernel::alloc::Box<T, Kmalloc>;
pub type KVBox<T> = kernel::alloc::Box<T, KVmalloc>;

// The kernel's, with the flags the task's memalloc scope says - so KVVec,
// KBox and everything built on them honour NORECLAIM and NOWARN, which the
// page allocator doesn't know.
macro_rules! allocators {
    ($($a:ident),*) => {$(
        unsafe impl Allocator for $a {
            const MIN_ALIGN: usize = <kernel::alloc::allocator::$a as Allocator>::MIN_ALIGN;

            unsafe fn realloc(ptr: Option<NonNull<u8>>, layout: Layout, old_layout: Layout, flags: Flags,
                              nid: NumaNode) -> Result<NonNull<[u8]>, AllocError>
            {
                unsafe {
                    <kernel::alloc::allocator::$a as Allocator>::realloc(
                        ptr, layout, old_layout, current_gfp_context(flags), nid)
                }
            }

            unsafe fn free(ptr: NonNull<u8>, layout: Layout) {
                unsafe { <kernel::alloc::allocator::$a as Allocator>::free(ptr, layout) }
            }
        }
    )*};
}

allocators!(Kmalloc, KVmalloc);

macro_rules! allocs {
    ($($a:ident => $nofail:expr),*) => {$(
        unsafe impl Alloc for $a {
            const KVFREE: bool = true;

            fn alloc(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
                <$a as Allocator>::alloc(layout, flags::GFP_KERNEL, NumaNode::NO_NODE)
            }

            fn alloc_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
                self.alloc_zeroed_gfp(layout, flags::GFP_KERNEL)
            }

            unsafe fn free(&self, p: NonNull<u8>, layout: Layout) {
                unsafe { <$a as Allocator>::free(p, layout) };
            }
        }

        unsafe impl AllocGfp for $a {
            fn alloc_zeroed_nofail(&self, layout: Layout) -> NonNull<[u8]> {
                let p = NonNull::new($nofail(layout.size()).cast()).expect("__GFP_NOFAIL allocation returned NULL");
                NonNull::slice_from_raw_parts(p, layout.size())
            }

            fn alloc_zeroed_gfp(&self, layout: Layout, gfp: Flags) -> Result<NonNull<[u8]>, AllocError> {
                use kernel::alloc::flags::__GFP_ZERO;

                <$a as Allocator>::alloc(layout, gfp | __GFP_ZERO, NumaNode::NO_NODE)
            }

            unsafe fn realloc(&self, p: Option<NonNull<u8>>, layout: Layout, old: Layout, gfp: Flags)
                -> Result<NonNull<[u8]>, AllocError>
            {
                unsafe { <$a as Allocator>::realloc(p, layout, old, gfp, NumaNode::NO_NODE) }
            }
        }
    )*};
}

allocs!(Kmalloc => |n| kzalloc_nofail(n), KVmalloc => |n| kvzalloc_nofail(n).as_ptr());

// The kernel's Box and Vec free as their T would.
macro_rules! held_pointers {
    ($($trait:ident),*) => {$(
        unsafe impl<T: ?Sized + $trait, A: Allocator> $trait for kernel::alloc::Box<T, A> {}
        unsafe impl<T: $trait, A: Allocator> $trait for kernel::alloc::kvec::Vec<T, A> {}
    )*};
}

held_pointers!(FreeAnytime, DropAnytime, DropAtomic, NothingToDetach);

/// @flags as the task's memalloc scope says, as the kernel's
/// current_gfp_context() does for NOIO and NOFS - which the page allocator
/// applies itself: NORECLAIM takes away direct reclaim, so GFP_KERNEL is
/// GFP_NOWAIT, and NOWARN adds __GFP_NOWARN.
pub fn current_gfp_context(flags: Flags) -> Flags {
    use crate::locking::{PF_MEMALLOC_NORECLAIM, PF_MEMALLOC_NOWARN};

    let pf = crate::os::sched::memalloc_flags();
    let mut flags = flags;
    if pf & PF_MEMALLOC_NORECLAIM != 0 {
        // __GFP_DIRECT_RECLAIM, which kernel::alloc doesn't name - and
        // __GFP_IO and __GFP_FS, which mean nothing without it
        flags = flags & !(flags::GFP_KERNEL & !flags::GFP_NOWAIT);
    }
    if pf & PF_MEMALLOC_NOWARN != 0 {
        flags = flags | flags::__GFP_NOWARN;
    }
    flags
}

/// current_gfp_context(), for what takes a C gfp_t.
pub fn current_gfp_context_raw(gfp: c::gfp_t) -> c::gfp_t {
    use crate::locking::{PF_MEMALLOC_NORECLAIM, PF_MEMALLOC_NOWARN};

    let pf = crate::os::sched::memalloc_flags();
    let mut gfp = gfp;
    if pf & PF_MEMALLOC_NORECLAIM != 0 {
        gfp &= !(c::GFP_KERNEL & !c::GFP_NOWAIT);
    }
    if pf & PF_MEMALLOC_NOWARN != 0 {
        gfp |= c::__GFP_NOWARN;
    }
    gfp
}

/// For an allocation that can't fail - a mempool's, that waits for an
/// element, or __GFP_NOFAIL: never in a NORECLAIM scope, where it would fail,
/// or lock up.
pub fn assert_may_reclaim() {
    ebug_assert!(crate::os::sched::memalloc_flags() & crate::locking::PF_MEMALLOC_NORECLAIM == 0,
                 "an allocation that can't fail, in a NORECLAIM scope");
}

/// The last ref's put frees it at once: what RCU readers may reach, which
/// isn't FreeAnytime, can't be in one.
pub fn arc_new<T: Send + Sync + FreeAnytime>(v: T) -> Result<Arc<T>, AllocError> {
    Arc::new(v, current_gfp_context(flags::GFP_KERNEL))
}

/// @b's value, out of its box: the kernel's Box has into_inner().
pub fn kbox_into_inner<T>(b: KBox<T>) -> T {
    KBox::into_inner(b)
}

/// @size bytes kzalloc()'d, __GFP_NOFAIL: never null. For C's kfree().
pub fn kzalloc_nofail(size: usize) -> *mut c_void {
    assert_may_reclaim();
    unsafe { c::rust_kzalloc_nofail(size) }
}

/// @size bytes kvzalloc()'d, __GFP_NOFAIL. For C's kvfree(), or kvfree_rcu().
pub fn kvzalloc_nofail(size: usize) -> NonNull<u8> {
    assert_may_reclaim();
    NonNull::new(unsafe { c::rust_kvzalloc_nofail(size) }.cast())
        .expect("__GFP_NOFAIL allocation returned NULL")
}

/// A zeroed KBox that can't fail to allocate: __GFP_NOFAIL, which the
/// kernel's Rust allocator API doesn't offer - so kzalloc()'d through C,
/// kmalloc()'s memory, what a KBox frees.
pub fn kbox_zeroed_nofail<T: ::pin_init::Zeroable + FreeAnytime>() -> KBox<T> {
    // kmalloc()'s alignment, at least
    const { assert!(size_of::<T>() != 0 && align_of::<T>() <= 8) };

    let p = kzalloc_nofail(size_of::<T>()).cast::<T>();
    assert!(!p.is_null(), "__GFP_NOFAIL allocation returned NULL");
    // SAFETY: kmalloc()'d, a T's size and alignment
    unsafe { KBox::from_raw(p) }
}

/// @layout's worth of memory, zeroed, as a KBox allocates: for a KBox of an
/// unsized type it can't make itself - a struct ending in a slice - by
/// KBox::from_raw(). Or kfree(), if it's never made.
pub fn kzalloc(layout: Layout) -> Result<NonNull<u8>, AllocError> {
    use kernel::alloc::flags::__GFP_ZERO;

    Ok(<Kmalloc as Allocator>::alloc(layout, flags::GFP_KERNEL | __GFP_ZERO, NumaNode::NO_NODE)?.cast())
}

/// # Safety
/// @p is kzalloc()'s, of @layout, and nothing uses it after.
pub unsafe fn kfree(p: NonNull<u8>, layout: Layout) {
    // SAFETY: the caller's
    unsafe { <Kmalloc as Allocator>::free(p, layout) };
}

/// @p - kmalloc()'d, or null - as @new bytes, what was there kept: krealloc(),
/// for a buffer C owns and kfree()s. Of no size the caller knows, as C's
/// aren't. GFP_KERNEL, the caller's memalloc scope saying the rest.
///
/// # Safety
/// @p is null, or kmalloc()'s and not used after unless this fails.
pub unsafe fn krealloc(p: *mut u8, new: usize) -> Result<NonNull<u8>, AllocError> {
    // Kmalloc::realloc() wants the old allocation's layout, which nobody has
    use kernel::bindings::{krealloc_node_align, GFP_KERNEL, NUMA_NO_NODE};

    // SAFETY: the caller's
    let gfp = current_gfp_context_raw(GFP_KERNEL);
    NonNull::new(unsafe { krealloc_node_align(p.cast(), new, 1, gfp, NUMA_NO_NODE) }.cast()).ok_or(AllocError)
}

/// @size bytes kmalloc()'d, uninitialized: for a buffer of no size anyone
/// keeps - C's kfree(), or kfree_unsized(), takes it back.
pub fn kmalloc(size: usize) -> Result<NonNull<u8>, AllocError> {
    let layout = Layout::from_size_align(size, 1).map_err(|_| AllocError)?;
    Ok(<Kmalloc as Allocator>::alloc(layout, flags::GFP_KERNEL, NumaNode::NO_NODE)?.cast())
}

/// kvzalloc(), with @gfp: for the rare allocation that needs a flag Flags
/// doesn't name - __GFP_NORETRY. kvfree_sized() takes it back.
pub fn kvzalloc_gfp(size: usize, gfp: c::gfp_t) -> Result<NonNull<u8>, AllocError> {
    use kernel::bindings::{kvrealloc_node_align, NUMA_NO_NODE};

    let layout = kv_layout(size)?;
    let gfp = current_gfp_context_raw(gfp | c::__GFP_ZERO);
    NonNull::new(unsafe {
        kvrealloc_node_align(core::ptr::null(), size, layout.align() as _, gfp, NUMA_NO_NODE)
    }.cast()).ok_or(AllocError)
}

/// KVmalloc's realloc(), with @gfp: for what needs a flag Flags doesn't name -
/// __GFP_NORETRY.
///
/// # Safety
/// As Alloc::realloc()'s.
pub unsafe fn kvmalloc_realloc_gfp(p: Option<NonNull<u8>>, layout: Layout, _old: Layout, gfp: c::gfp_t)
    -> Result<NonNull<u8>, AllocError>
{
    use kernel::bindings::{kvrealloc_node_align, NUMA_NO_NODE};

    let p = p.map_or(core::ptr::null(), |p| p.as_ptr().cast_const().cast());
    NonNull::new(unsafe {
        kvrealloc_node_align(p, layout.size(), layout.align() as _, current_gfp_context_raw(gfp), NUMA_NO_NODE)
    }.cast()).ok_or(AllocError)
}

/// @p, @old bytes of kvmalloc()'d memory - or none, null - as @new bytes,
/// what was there kept and the rest zeroed: kvrealloc(), __GFP_ZERO.
/// Aligned as kmalloc() aligns @new. kvfree_sized() takes it back.
pub fn kvrealloc_zeroed(p: *mut u8, old: usize, new: usize) -> Result<NonNull<u8>, AllocError> {
    use kernel::alloc::flags::__GFP_ZERO;

    // SAFETY: @p is KVmalloc's, of @old bytes, or null
    Ok(unsafe {
        <KVmalloc as Allocator>::realloc(NonNull::new(p), kv_layout(new)?, kv_layout(old)?,
                                         flags::GFP_KERNEL | __GFP_ZERO, NumaNode::NO_NODE)
    }?.cast())
}

/// @p, @old bytes of kvmalloc()'d memory - or none, null - as @new bytes,
/// what was there kept: kvrealloc(). Aligned as kmalloc() aligns @new;
/// kvfree_sized() takes it back.
pub fn kvrealloc(p: *mut u8, old: usize, new: usize) -> Result<NonNull<u8>, AllocError> {
    // SAFETY: @p is KVmalloc's, of @old bytes, or null
    Ok(unsafe {
        <KVmalloc as Allocator>::realloc(NonNull::new(p), kv_layout(new)?, kv_layout(old)?,
                                         flags::GFP_KERNEL, NumaNode::NO_NODE)
    }?.cast())
}

/// What a kvmalloc() reallocation that mustn't fail falls back to, a plain
/// one having failed: @new bytes, __GFP_NOFAIL - through C, as the kernel's
/// Rust has no __GFP_NOFAIL - @p's @old copied in, and @p freed. Aligned as
/// kmalloc() aligns @new.
///
/// # Safety
/// @p is kvmalloc()'d, of @old bytes, and nothing uses it after.
pub unsafe fn kvrealloc_nofail_fallback(p: Option<NonNull<u8>>, old: usize, new: usize) -> NonNull<u8> {
    let q = kvzalloc_nofail(new);
    if let Some(p) = p {
        unsafe {
            q.as_ptr().copy_from_nonoverlapping(p.as_ptr(), old.min(new));
            <KVmalloc as Allocator>::free(p, kv_layout(old).expect("its allocation's"));
        }
    }
    q
}

/// # Safety
/// @p is kvrealloc_zeroed()'s or kvrealloc()'s, of @size bytes, and nothing
/// uses it after.
pub unsafe fn kvfree_sized(p: NonNull<u8>, size: usize) {
    let layout = kv_layout(size).expect("its allocation's");
    // SAFETY: the caller's
    unsafe { <KVmalloc as Allocator>::free(p, layout) };
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

/// @size bytes of pages, with @gfp, for a bio's data: page aligned, as we
/// don't know the device's DMA alignment.
pub fn bio_pages_alloc(size: usize, gfp: c::gfp_t) -> *mut c_void {
    unsafe { c::rust_bio_pages_alloc(size, gfp) }
}

/// # Safety
/// @p is bio_pages_alloc()'s, of @size bytes, and nothing uses it after.
pub unsafe fn bio_pages_free(p: *mut c_void, size: usize) {
    unsafe { c::rust_bio_pages_free(p, size) }
}

// ── Aligned boxes ────────────────────────────────────────────────────────
//
// ABox<T, A>, and AVec<T, A> to build one, aligned-vec's API, over kvmalloc -
// the part of it that's used: the kernel has no global allocator for the
// crate's. Memory's as the task's memalloc scope allows: no flags.

pub use abox::{ABox, AVec, Alignment, ConstAlign};

mod abox {
    use super::AllocError;
    use core::alloc::Layout;
    use core::marker::PhantomData;
    use core::ptr::NonNull;
    use crate::util::alloc::KVmalloc;
    use kernel::alloc::{flags::GFP_KERNEL, Allocator, NumaNode};

    /// An alignment, as aligned-vec's: here only the compile-time one.
    pub trait Alignment {
        const ALIGN: usize;
    }

    #[derive(Clone, Copy)]
    pub struct ConstAlign<const ALIGN: usize>;

    impl<const N: usize> Alignment for ConstAlign<N> {
        const ALIGN: usize = N;
    }

    pub struct AVec<T, A: Alignment> {
        p:   NonNull<T>,
        len: usize,
        cap: usize,
        _a:  PhantomData<A>,
    }

    // SAFETY: owns its Ts, as a Vec does
    unsafe impl<T: Send, A: Alignment> Send for AVec<T, A> {}
    unsafe impl<T: Sync, A: Alignment> Sync for AVec<T, A> {}

    fn layout<T, A: Alignment>(n: usize) -> Result<Layout, AllocError> {
        Layout::array::<T>(n).and_then(|l| l.align_to(A::ALIGN)).map_err(|_| AllocError)
    }

    impl<T, A: Alignment> AVec<T, A> {
        /// Empty: room's made as it's reserved. @align is A's, as the crate's.
        pub fn new(_align: usize) -> Self {
            const { assert!(size_of::<T>() != 0, "an AVec of a zero sized type") };
            AVec { p: NonNull::dangling(), len: 0, cap: 0, _a: PhantomData }
        }

        pub fn len(&self) -> usize {
            self.len
        }

        /// Room for exactly @additional more.
        pub fn try_reserve_exact(&mut self, additional: usize) -> Result<(), AllocError> {
            if additional <= self.cap - self.len {
                return Ok(());
            }
            let cap = self.len.checked_add(additional).ok_or(AllocError)?;
            let old = (self.cap != 0).then_some(self.p.cast());
            // SAFETY: @old is KVmalloc's, of its layout, or none
            let p = unsafe {
                KVmalloc::realloc(old, layout::<T, A>(cap)?, layout::<T, A>(self.cap)?, GFP_KERNEL, NumaNode::NO_NODE)
            }?;
            self.p = p.cast();
            self.cap = cap;
            Ok(())
        }

        /// # Safety
        /// @len is within its capacity, and that many of it initialized.
        pub unsafe fn set_len(&mut self, len: usize) {
            self.len = len;
        }

        /// As a boxed slice. Not one with room left: the crate's shrinks to
        /// fit, which nothing here needs.
        pub fn into_boxed_slice(self) -> ABox<[T], A> {
            assert_eq!(self.len, self.cap, "into_boxed_slice() of an AVec with room left");
            let this = core::mem::ManuallyDrop::new(self);
            ABox { p: NonNull::slice_from_raw_parts(this.p, this.len), _a: PhantomData }
        }
    }

    impl<T, A: Alignment> Drop for AVec<T, A> {
        fn drop(&mut self) {
            // SAFETY: its @len, initialized; its memory, of @cap, KVmalloc's
            unsafe {
                core::ptr::slice_from_raw_parts_mut(self.p.as_ptr(), self.len).drop_in_place();
                if self.cap != 0 {
                    KVmalloc::free(self.p.cast(), layout::<T, A>(self.cap).expect("its allocation's"));
                }
            }
        }
    }

    pub struct ABox<T: ?Sized, A: Alignment> {
        p:  NonNull<T>,
        _a: PhantomData<A>,
    }

    // SAFETY: owns its T, as a Box does
    unsafe impl<T: ?Sized + Send, A: Alignment> Send for ABox<T, A> {}
    unsafe impl<T: ?Sized + Sync, A: Alignment> Sync for ABox<T, A> {}

    impl<T: ?Sized, A: Alignment> core::ops::Deref for ABox<T, A> {
        type Target = T;

        fn deref(&self) -> &T {
            // SAFETY: its own, live
            unsafe { self.p.as_ref() }
        }
    }

    impl<T: ?Sized, A: Alignment> core::ops::DerefMut for ABox<T, A> {
        fn deref_mut(&mut self) -> &mut T {
            // SAFETY: its own, live, borrowed mutably
            unsafe { self.p.as_mut() }
        }
    }

    impl<T: ?Sized, A: Alignment> Drop for ABox<T, A> {
        fn drop(&mut self) {
            // SAFETY: its own, live; KVmalloc's, laid out as it is, aligned to A
            unsafe {
                let layout = Layout::for_value(self.p.as_ref()).align_to(A::ALIGN).expect("its allocation's");
                self.p.as_ptr().drop_in_place();
                if layout.size() != 0 {
                    KVmalloc::free(self.p.cast(), layout);
                }
            }
        }
    }
}
