// SPDX-License-Identifier: GPL-2.0

//! The kernel's allocation, for util/alloc.rs: Kmalloc, Vmalloc and KVmalloc
//! are C's krealloc(), vrealloc() and kvrealloc(), with our Flags as the
//! task's memalloc scope says - and the kernel's Rust Allocator, for its Box
//! and Arc, the same. What the kernel's Rust doesn't have - a kfree() of no
//! size - is through shim.c.

#[path = "../alloc.rs"]
mod common;
pub use common::*;

pub use kernel::alloc::AllocError;
pub use kernel::sync::Arc;

use crate::c;
use crate::util::alloc::{kv_layout, Alloc, AllocGfp, KVmalloc, Kmalloc, Vmalloc};
use crate::util::rcu::{DropAnytime, DropAtomic, FreeAnytime, NothingToDetach};
use core::alloc::Layout;
use core::ffi::c_void;
use core::ptr::NonNull;
use kernel::alloc::{Allocator, NumaNode};

pub type KBox<T> = kernel::alloc::Box<T, Kmalloc>;
pub type KVBox<T> = kernel::alloc::Box<T, KVmalloc>;

impl Kmalloc {
    pub const MAX_SIZE: usize = super::kernel::PAGE_SIZE << c::MAX_PAGE_ORDER;
}

impl Flags {
    /// The kernel's gfp_t of them.
    pub const fn raw(self) -> c::gfp_t {
        const GFP: [(Flags, c::gfp_t); 13] = [
            (flags::DIRECT_RECLAIM,     c::__GFP_DIRECT_RECLAIM),
            (flags::BACKGROUND_RECLAIM, c::__GFP_KSWAPD_RECLAIM),
            (flags::IO,                 c::__GFP_IO),
            (flags::FS,                 c::__GFP_FS),
            (flags::HIGH_PRIORITY,      c::__GFP_HIGH),
            (flags::ZERO,               c::__GFP_ZERO),
            (flags::NOWARN,             c::__GFP_NOWARN),
            (flags::NORETRY,            c::__GFP_NORETRY),
            (flags::NO_RESERVES,        c::__GFP_NOMEMALLOC),
            (flags::ACCOUNT,            c::__GFP_ACCOUNT),
            (flags::RECLAIMABLE,        c::__GFP_RECLAIMABLE),
            (flags::SKIP_ZERO,          c::__GFP_SKIP_ZERO),
            (flags::NOFAIL,             c::__GFP_NOFAIL),
        ];

        let mut gfp = 0;
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
        unsafe impl Allocator for $a {
            const MIN_ALIGN: usize = <kernel::alloc::allocator::$a as Allocator>::MIN_ALIGN;

            unsafe fn realloc(ptr: Option<NonNull<u8>>, layout: Layout, old_layout: Layout,
                              flags: kernel::alloc::Flags, nid: NumaNode) -> Result<NonNull<[u8]>, AllocError>
            {
                unsafe {
                    <kernel::alloc::allocator::$a as Allocator>::realloc(
                        ptr, layout, old_layout, kernel_gfp_context(flags), nid)
                }
            }

            unsafe fn free(ptr: NonNull<u8>, layout: Layout) {
                unsafe { <kernel::alloc::allocator::$a as Allocator>::free(ptr, layout) }
            }
        }
    )*};
}

allocators!(Kmalloc, Vmalloc, KVmalloc);

macro_rules! allocs {
    ($($a:ident => $realloc:ident, $layout:expr),*) => {$(
        unsafe impl Alloc for $a {
            const KVFREE: bool = true;

            fn alloc(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
                unsafe { self.realloc(None, layout, layout, flags::WAIT) }
            }

            fn alloc_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
                self.alloc_zeroed_gfp(layout, flags::WAIT)
            }

            unsafe fn free(&self, p: NonNull<u8>, layout: Layout) {
                unsafe { <$a as Allocator>::free(p, layout) };
            }
        }

        unsafe impl AllocGfp for $a {
            fn alloc_zeroed_gfp(&self, layout: Layout, gfp: Flags) -> Result<NonNull<[u8]>, AllocError> {
                unsafe { self.realloc(None, layout, layout, gfp | flags::ZERO) }
            }

            /// C's krealloc(), vrealloc() or kvrealloc(), as the kernel's
            /// Rust allocator calls them, with @gfp as the task's memalloc
            /// scope says.
            unsafe fn realloc(&self, p: Option<NonNull<u8>>, layout: Layout, old: Layout, gfp: Flags)
                -> Result<NonNull<[u8]>, AllocError>
            {
                use kernel::bindings::{$realloc, NUMA_NO_NODE};

                let layout: Layout = $layout(layout);
                // a zero-sized allocation is dangling: nothing for C to free
                let p = p.filter(|_| old.size() != 0).map_or(core::ptr::null(), |p| p.as_ptr().cast_const().cast());
                let gfp = current_gfp_context(gfp).raw();
                let q = unsafe { $realloc(p, layout.size(), layout.align() as _, gfp, NUMA_NO_NODE) };
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
allocs!(Kmalloc  => krealloc_node_align,  |l: Layout| l.pad_to_align(),
        Vmalloc  => vrealloc_node_align,  |l: Layout| l,
        KVmalloc => kvrealloc_node_align, |l: Layout| l.pad_to_align());

// The kernel's Box and Vec free as their T would.
macro_rules! held_pointers {
    ($($trait:ident),*) => {$(
        unsafe impl<T: ?Sized + $trait, A: Allocator> $trait for kernel::alloc::Box<T, A> {}
        unsafe impl<T: $trait, A: Allocator> $trait for kernel::alloc::kvec::Vec<T, A> {}
    )*};
}

held_pointers!(FreeAnytime, DropAnytime, DropAtomic, NothingToDetach);

/// What the task's memalloc scope takes from an allocation's flags, and what
/// it adds - the scopes the page allocator doesn't apply itself, as it does
/// NOIO and NOFS: NORECLAIM takes direct reclaim, and the IO and FS that mean
/// nothing without it; NOWARN adds NOWARN.
fn memalloc_scope() -> (Flags, Flags) {
    use crate::locking::{PF_MEMALLOC_NORECLAIM, PF_MEMALLOC_NOWARN};

    let pf = crate::os::sched::memalloc_flags();
    let take = if pf & PF_MEMALLOC_NORECLAIM != 0 {
        flags::DIRECT_RECLAIM | flags::IO | flags::FS
    } else {
        Flags::empty()
    };
    let add = if pf & PF_MEMALLOC_NOWARN != 0 { flags::NOWARN } else { Flags::empty() };
    (take, add)
}

/// @gfp as the task's memalloc scope says.
pub fn current_gfp_context(gfp: Flags) -> Flags {
    let (take, add) = memalloc_scope();
    ebug_assert!(!gfp.contains(flags::NOFAIL) || take == Flags::empty(),
                 "a NOFAIL allocation, in a NORECLAIM scope");
    gfp & !take | add
}

/// current_gfp_context(), for a gfp_t of the kernel's own - a mapping's, which
/// the VFS hands us.
pub fn current_gfp_context_raw(gfp: c::gfp_t) -> c::gfp_t {
    let (take, add) = memalloc_scope();
    gfp & !take.raw() | add.raw()
}

/// current_gfp_context(), for the kernel's Rust Flags - its Box's and Arc's -
/// which name no direct reclaim, IO or FS: GFP_KERNEL less GFP_NOWAIT is
/// those.
fn kernel_gfp_context(f: kernel::alloc::Flags) -> kernel::alloc::Flags {
    use kernel::alloc::flags::{GFP_KERNEL, GFP_NOWAIT, __GFP_NOWARN};

    let (take, add) = memalloc_scope();
    let f = if take != Flags::empty() { f & !(GFP_KERNEL & !GFP_NOWAIT) } else { f };
    if add != Flags::empty() { f | __GFP_NOWARN } else { f }
}

/// The last ref's put frees it at once: what RCU readers may reach, which
/// isn't FreeAnytime, can't be in one.
pub fn arc_new<T: Send + Sync + FreeAnytime>(v: T) -> Result<Arc<T>, AllocError> {
    Arc::new(v, kernel_gfp_context(kernel::alloc::flags::GFP_KERNEL))
}

/// @b's value, out of its box: the kernel's Box has into_inner().
pub fn kbox_into_inner<T>(b: KBox<T>) -> T {
    KBox::into_inner(b)
}

/// @size bytes kzalloc()'d, NOFAIL: never null. kfree_unsized() takes it back.
pub fn kzalloc_nofail(size: usize) -> *mut c_void {
    kzalloc_gfp(size, flags::WAIT | flags::NOFAIL).expect("a NOFAIL allocation failed").as_ptr().cast()
}

/// @size bytes kvzalloc()'d, NOFAIL. kvfree_unsized() takes it back, or
/// kvfree_rcu_head().
pub fn kvzalloc_nofail(size: usize) -> NonNull<u8> {
    kvzalloc_gfp(size, flags::WAIT | flags::NOFAIL).expect("a NOFAIL allocation failed")
}

/// A zeroed KBox that can't fail to allocate: Kmalloc's, NOFAIL - which the
/// kernel's Rust Box can't ask for.
pub fn kbox_zeroed_nofail<T: ::pin_init::Zeroable + FreeAnytime>() -> KBox<T> {
    let p = Kmalloc.alloc_zeroed_gfp(Layout::new::<T>(), flags::WAIT | flags::NOFAIL)
        .expect("a NOFAIL allocation failed");
    unsafe { KBox::from_raw(p.as_ptr().cast::<T>()) }
}

/// @layout's worth of memory, zeroed, as a KBox allocates: for a KBox of an
/// unsized type it can't make itself - a struct ending in a slice - by
/// KBox::from_raw(). Or kfree(), if it's never made.
pub fn kzalloc(layout: Layout) -> Result<NonNull<u8>, AllocError> {
    Ok(Kmalloc.alloc_zeroed(layout)?.cast())
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
    use kernel::bindings::{krealloc_node_align, NUMA_NO_NODE};

    // SAFETY: the caller's
    let gfp = current_gfp_context(flags::WAIT).raw();
    NonNull::new(unsafe { krealloc_node_align(p.cast(), new, 1, gfp, NUMA_NO_NODE) }.cast()).ok_or(AllocError)
}

/// krealloc()'s, kvmalloc()'d: what C's kvfree() takes back.
///
/// # Safety
/// @p is null, or kvmalloc()'s and not used after unless this fails.
pub unsafe fn kvrealloc_unsized(p: *mut u8, new: usize) -> Result<NonNull<u8>, AllocError> {
    use kernel::bindings::{kvrealloc_node_align, NUMA_NO_NODE};

    let gfp = current_gfp_context(flags::WAIT).raw();
    NonNull::new(unsafe { kvrealloc_node_align(p.cast(), new, 1, gfp, NUMA_NO_NODE) }.cast()).ok_or(AllocError)
}

/// @size bytes kmalloc()'d, uninitialized: for a buffer of no size anyone
/// keeps - C's kfree(), or kfree_unsized(), takes it back.
pub fn kmalloc(size: usize) -> Result<NonNull<u8>, AllocError> {
    let layout = Layout::from_size_align(size, 1).map_err(|_| AllocError)?;
    Ok(Kmalloc.alloc(layout)?.cast())
}

/// kzalloc(), with @gfp. kfree_unsized() takes it back.
pub fn kzalloc_gfp(size: usize, gfp: Flags) -> Result<NonNull<u8>, AllocError> {
    let layout = Layout::from_size_align(size, 1).map_err(|_| AllocError)?;
    Ok(Kmalloc.alloc_zeroed_gfp(layout, gfp)?.cast())
}

/// kvzalloc(), with @gfp. kvfree_sized() takes it back.
pub fn kvzalloc_gfp(size: usize, gfp: Flags) -> Result<NonNull<u8>, AllocError> {
    Ok(KVmalloc.alloc_zeroed_gfp(kv_layout(size)?, gfp)?.cast())
}

/// @p, @old bytes of kvmalloc()'d memory - or none, null - as @new bytes,
/// what was there kept and the rest zeroed: kvrealloc(), __GFP_ZERO.
/// Aligned as kmalloc() aligns @new. kvfree_sized() takes it back.
pub fn kvrealloc_zeroed(p: *mut u8, old: usize, new: usize) -> Result<NonNull<u8>, AllocError> {
    // SAFETY: @p is KVmalloc's, of @old bytes, or null
    Ok(unsafe { KVmalloc.realloc(NonNull::new(p), kv_layout(new)?, kv_layout(old)?, flags::WAIT | flags::ZERO) }?.cast())
}

/// @p, @old bytes of kvmalloc()'d memory - or none, null - as @new bytes,
/// what was there kept: kvrealloc(), with @gfp. Aligned as kmalloc() aligns
/// @new; kvfree_sized() takes it back.
pub fn kvrealloc(p: *mut u8, old: usize, new: usize, gfp: Flags) -> Result<NonNull<u8>, AllocError> {
    // SAFETY: @p is KVmalloc's, of @old bytes, or null
    Ok(unsafe { KVmalloc.realloc(NonNull::new(p), kv_layout(new)?, kv_layout(old)?, gfp) }?.cast())
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
pub fn bio_pages_alloc(size: usize, gfp: Flags) -> *mut c_void {
    unsafe { c::rust_bio_pages_alloc(size, gfp.raw()) }
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
    use crate::util::alloc::{flags, AllocGfp, KVmalloc};
    use kernel::alloc::Allocator;

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
            let p = unsafe { KVmalloc.realloc(old, layout::<T, A>(cap)?, layout::<T, A>(self.cap)?, flags::WAIT) }?;
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
                    <KVmalloc as Allocator>::free(self.p.cast(), layout::<T, A>(self.cap).expect("its allocation's"));
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
                    <KVmalloc as Allocator>::free(self.p.cast(), layout);
                }
            }
        }
    }
}
