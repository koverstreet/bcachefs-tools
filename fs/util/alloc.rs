// SPDX-License-Identifier: GPL-2.0

//! Fallible allocation for code shared by both builds: in-kernel, the
//! kernel's kernel::alloc over allocators of ours, which apply the task's
//! memalloc scope; bcachefs-shim's copy of the same API in userspace. An
//! allocation failure is an AllocError to handle, never an abort.

#[cfg(kernel)]
pub use kernel::alloc::{flags, AllocError, Flags};

#[cfg(not(kernel))]
pub use bcachefs_shim::alloc::{flags, AllocError, Flags};

use core::alloc::Layout;
use core::ptr::NonNull;
/// In userspace, a Box is a KBox.
#[cfg(not(kernel))]
pub type KBox<T> = std::boxed::Box<T>;
/// And a KVBox.
#[cfg(not(kernel))]
pub type KVBox<T> = std::boxed::Box<T>;

#[cfg(kernel)]
pub use kernel::sync::Arc;
#[cfg(not(kernel))]
pub use std::sync::Arc;

use crate::util::maybe_rcu_alloc::MaybeRcuAlloc;
use crate::util::rcu::FreeAnytime;

// ── Allocators ───────────────────────────────────────────────────────────
//
// The kernel's, with the flags the task's memalloc scope says - so KVVec,
// KBox and everything built on them honour NORECLAIM and NOWARN, which the
// page allocator doesn't know. In userspace, std's allocator takes no flags.

pub struct Kmalloc;
pub struct KVmalloc;

#[cfg(kernel)]
macro_rules! allocators {
    ($($a:ident),*) => {$(
        unsafe impl kernel::alloc::Allocator for $a {
            const MIN_ALIGN: usize = <kernel::alloc::allocator::$a as kernel::alloc::Allocator>::MIN_ALIGN;

            unsafe fn realloc(ptr: Option<core::ptr::NonNull<u8>>, layout: core::alloc::Layout,
                              old_layout: core::alloc::Layout, flags: Flags, nid: kernel::alloc::NumaNode)
                -> Result<core::ptr::NonNull<[u8]>, AllocError>
            {
                unsafe {
                    <kernel::alloc::allocator::$a as kernel::alloc::Allocator>::realloc(
                        ptr, layout, old_layout, current_gfp_context(flags), nid)
                }
            }

            unsafe fn free(ptr: core::ptr::NonNull<u8>, layout: core::alloc::Layout) {
                unsafe { <kernel::alloc::allocator::$a as kernel::alloc::Allocator>::free(ptr, layout) }
            }
        }
    )*};
}

#[cfg(kernel)]
allocators!(Kmalloc, KVmalloc);

/// How an allocation may fail: as @Flags let it - or, for what mustn't, not
/// at all: a plain GFP_KERNEL allocation first, then __GFP_NOFAIL.
#[derive(Clone, Copy)]
pub enum AllocMode {
    Gfp(Flags),
    Nofail,
}

/// An allocator, as both builds have it: in-kernel, ours above - the task's
/// memalloc scope applied - and in userspace, std's, which takes no flags.
///
/// # Safety
/// alloc() gives @layout's bytes, zeroed and aligned to it; realloc() the
/// new layout's, what was there kept up to the smaller of the two; free(),
/// and in-kernel kvfree(), free them, from any context.
pub unsafe trait Alloc {
    fn alloc(layout: Layout, gfp: Flags) -> Result<NonNull<u8>, AllocError>;

    /// @p - None for a new allocation - as @layout, not zeroed: @p freed if
    /// it moves, and kept if this fails.
    ///
    /// # Safety
    /// @p is this allocator's, of @old.
    unsafe fn realloc(p: Option<NonNull<u8>>, layout: Layout, old: Layout, gfp: Flags)
        -> Result<NonNull<u8>, AllocError>;

    /// # Safety
    /// @p is alloc()'s or realloc()'s, of @layout, and nothing uses it after.
    unsafe fn free(p: NonNull<u8>, layout: Layout);

    /// alloc(), __GFP_NOFAIL: through C, as the kernel's Rust has no
    /// __GFP_NOFAIL - aligned as kmalloc() aligns, to 8. Userspace's
    /// allocator aborts rather than fail.
    fn alloc_nofail(layout: Layout) -> NonNull<u8>;

    fn alloc_mode(layout: Layout, mode: AllocMode) -> Result<NonNull<u8>, AllocError> {
        match mode {
            AllocMode::Gfp(gfp) => Self::alloc(layout, gfp),
            AllocMode::Nofail => {
                // checked whether the fallback's needed or not: a misaligned
                // caller would otherwise find out only under memory pressure
                assert!(layout.align() <= 8, "a __GFP_NOFAIL allocation aligned past kmalloc()'s");
                Ok(Self::alloc(layout, flags::GFP_KERNEL).unwrap_or_else(|_| Self::alloc_nofail(layout)))
            }
        }
    }
}

macro_rules! allocs {
    ($($a:ident => $nofail:expr),*) => {$(
        unsafe impl Alloc for $a {
            fn alloc_nofail(layout: Layout) -> NonNull<u8> {
                #[cfg(kernel)]
                {
                    NonNull::new($nofail(layout.size()).cast()).expect("__GFP_NOFAIL allocation returned NULL")
                }
                #[cfg(not(kernel))]
                {
                    NonNull::new(unsafe { std::alloc::alloc_zeroed(layout) })
                        .unwrap_or_else(|| std::alloc::handle_alloc_error(layout))
                }
            }

            fn alloc(layout: Layout, gfp: Flags) -> Result<NonNull<u8>, AllocError> {
                #[cfg(kernel)]
                {
                    use kernel::alloc::{flags::__GFP_ZERO, Allocator, NumaNode};

                    Ok(<$a as Allocator>::alloc(layout, gfp | __GFP_ZERO, NumaNode::NO_NODE)?.cast())
                }
                #[cfg(not(kernel))]
                {
                    let _ = gfp;
                    NonNull::new(unsafe { std::alloc::alloc_zeroed(layout) }).ok_or(AllocError)
                }
            }

            unsafe fn realloc(p: Option<NonNull<u8>>, layout: Layout, old: Layout, gfp: Flags)
                -> Result<NonNull<u8>, AllocError>
            {
                #[cfg(kernel)]
                {
                    use kernel::alloc::{Allocator, NumaNode};

                    Ok(unsafe { <$a as Allocator>::realloc(p, layout, old, gfp, NumaNode::NO_NODE) }?.cast())
                }
                #[cfg(not(kernel))]
                {
                    let _ = gfp;
                    let p = match p {
                        Some(p) => unsafe { std::alloc::realloc(p.as_ptr(), old, layout.size()) },
                        None    => unsafe { std::alloc::alloc(layout) },
                    };
                    NonNull::new(p).ok_or(AllocError)
                }
            }

            unsafe fn free(p: NonNull<u8>, layout: Layout) {
                #[cfg(kernel)]
                unsafe { <$a as kernel::alloc::Allocator>::free(p, layout) };
                #[cfg(not(kernel))]
                unsafe { std::alloc::dealloc(p.as_ptr(), layout) };
            }
        }
    )*};
}

allocs!(Kmalloc => |n| kzalloc_nofail(n), KVmalloc => |n| kvzalloc_nofail(n).as_ptr());

#[cfg(kernel)]
pub type KBox<T> = kernel::alloc::Box<T, Kmalloc>;
#[cfg(kernel)]
pub type KVBox<T> = kernel::alloc::Box<T, KVmalloc>;

pub use crate::data_structures::vec::{DrainAll, IntoIter, PushError, Vec};

pub type KVec<T> = Vec<T, Kmalloc>;
pub type KVVec<T> = Vec<T, KVmalloc>;

/// @flags as the task's memalloc scope says, as the kernel's
/// current_gfp_context() does for NOIO and NOFS - which the page allocator
/// applies itself: NORECLAIM takes away direct reclaim, so GFP_KERNEL is
/// GFP_NOWAIT, and NOWARN adds __GFP_NOWARN.
pub fn current_gfp_context(flags: Flags) -> Flags {
    #[cfg(kernel)]
    {
        use crate::locking::{memalloc_flags, PF_MEMALLOC_NORECLAIM, PF_MEMALLOC_NOWARN};

        let pf = memalloc_flags();
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
    #[cfg(not(kernel))]
    flags
}

/// current_gfp_context(), for what takes a C gfp_t.
pub fn current_gfp_context_raw(gfp: crate::c::gfp_t) -> crate::c::gfp_t {
    #[cfg(kernel)]
    {
        use crate::c;
        use crate::locking::{memalloc_flags, PF_MEMALLOC_NORECLAIM, PF_MEMALLOC_NOWARN};

        let pf = memalloc_flags();
        let mut gfp = gfp;
        if pf & PF_MEMALLOC_NORECLAIM != 0 {
            gfp &= !(c::GFP_KERNEL & !c::GFP_NOWAIT);
        }
        if pf & PF_MEMALLOC_NOWARN != 0 {
            gfp |= c::__GFP_NOWARN;
        }
        gfp
    }
    #[cfg(not(kernel))]
    gfp
}

/// For an allocation that can't fail - a mempool's, that waits for an
/// element, or __GFP_NOFAIL: never in a NORECLAIM scope, where it would fail,
/// or lock up.
pub fn assert_may_reclaim() {
    #[cfg(kernel)]
    ebug_assert!(crate::locking::memalloc_flags() & crate::locking::PF_MEMALLOC_NORECLAIM == 0,
                 "an allocation that can't fail, in a NORECLAIM scope");
}

/// The last ref's put frees it at once: what RCU readers may reach, which
/// isn't FreeAnytime, can't be in one.
pub fn arc_new<T: Send + Sync + FreeAnytime>(v: T) -> Result<Arc<T>, AllocError> {
    #[cfg(kernel)]
    { Arc::new(v, current_gfp_context(flags::GFP_KERNEL)) }
    #[cfg(not(kernel))]
    { Ok(Arc::new(v)) }
}

/// A @T for C to hold: a MaybeRcuAlloc's, kvmalloc()'d, given up to a raw
/// pointer, which free_pinned() takes back.
pub fn alloc_pinned<T, E: From<AllocError>>(init: impl ::pin_init::PinInit<T, E>) -> Result<*mut T, E> {
    MaybeRcuAlloc::<T, KVmalloc>::new(init).map(|b| b.into_raw().as_ptr())
}

/// alloc_pinned(), the allocation failing with @err - @init with its own.
pub fn alloc_pinned_or<T>(init: impl ::pin_init::PinInit<T, crate::errcode::BchError>, err: crate::errcode::bch_errcode)
    -> Result<*mut T, crate::errcode::BchError>
{
    enum Failed {
        Alloc,
        Init(crate::errcode::BchError),
    }
    impl From<AllocError> for Failed {
        fn from(_: AllocError) -> Self {
            Failed::Alloc
        }
    }

    let init = unsafe { ::pin_init::pin_init_from_closure(move |slot| init_at(init, slot).map_err(Failed::Init)) };
    alloc_pinned(init).map_err(|e| match e {
        Failed::Alloc   => err.into(),
        Failed::Init(e) => e,
    })
}

/// Drop and free @p, as its MaybeRcuAlloc's drop does.
///
/// # Safety
/// @p is from alloc_pinned(), and nothing but RCU readers uses it after.
pub unsafe fn free_pinned<T>(p: *mut T) {
    drop(unsafe { MaybeRcuAlloc::<T, KVmalloc>::from_raw(core::ptr::NonNull::new_unchecked(p)) })
}

/// @b's value, out of its box: *b, for a std Box - which the kernel's isn't,
/// so it has into_inner().
pub fn kbox_into_inner<T>(b: KBox<T>) -> T {
    #[cfg(kernel)]
    { KBox::into_inner(b) }
    #[cfg(not(kernel))]
    { *b }
}

/// @size bytes kzalloc()'d, __GFP_NOFAIL: never null. For C's kfree().
pub fn kzalloc_nofail(size: usize) -> *mut core::ffi::c_void {
    assert_may_reclaim();
    unsafe { crate::c::rust_kzalloc_nofail(size) }
}

/// @size bytes kvzalloc()'d, __GFP_NOFAIL. For C's kvfree(), or kvfree_rcu().
pub fn kvzalloc_nofail(size: usize) -> core::ptr::NonNull<u8> {
    assert_may_reclaim();
    core::ptr::NonNull::new(unsafe { crate::c::rust_kvzalloc_nofail(size) }.cast())
        .expect("__GFP_NOFAIL allocation returned NULL")
}

/// A zeroed KBox that can't fail to allocate: __GFP_NOFAIL, which the
/// kernel's Rust allocator API doesn't offer - so kzalloc()'d through C,
/// kmalloc()'s memory, what a KBox frees. Userspace's allocator aborts rather
/// than fail.
pub fn kbox_zeroed_nofail<T: ::pin_init::Zeroable + FreeAnytime>() -> KBox<T> {
    // kmalloc()'s alignment, at least
    const { assert!(size_of::<T>() != 0 && align_of::<T>() <= 8) };

    #[cfg(kernel)]
    {
        let p = kzalloc_nofail(size_of::<T>()).cast::<T>();
        assert!(!p.is_null(), "__GFP_NOFAIL allocation returned NULL");
        // SAFETY: kmalloc()'d, a T's size and alignment
        unsafe { KBox::from_raw(p) }
    }
    #[cfg(not(kernel))]
    {
        let layout = core::alloc::Layout::new::<T>();
        // SAFETY: not zero sized
        let p = unsafe { std::alloc::alloc_zeroed(layout) }.cast::<T>();
        if p.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        // SAFETY: the global allocator's, of a T's layout; zeroed is a valid T
        unsafe { KBox::from_raw(p) }
    }
}

/// A KBox frees when it drops, without waiting out RCU readers: what they may
/// reach, which isn't FreeAnytime, can't be in one.
pub fn kbox_pin_init<T: FreeAnytime, E: From<AllocError>>(init: impl ::pin_init::PinInit<T, E>)
    -> Result<core::pin::Pin<KBox<T>>, E>
{
    let p = crate::util::maybe_rcu_alloc::alloc_at::<T, Kmalloc, E>(init, AllocMode::Gfp(flags::GFP_KERNEL))?;
    Ok(unsafe { core::pin::Pin::new_unchecked(KBox::from_raw(p.as_ptr())) })
}

/// @layout's worth of memory, zeroed, as a KBox allocates: for a KBox of an
/// unsized type it can't make itself - a struct ending in a slice - by
/// KBox::from_raw(). Or kfree(), if it's never made.
pub fn kzalloc(layout: core::alloc::Layout) -> Result<core::ptr::NonNull<u8>, AllocError> {
    #[cfg(kernel)]
    {
        use kernel::alloc::{flags::__GFP_ZERO, Allocator, NumaNode};

        Ok(<Kmalloc as Allocator>::alloc(layout, flags::GFP_KERNEL | __GFP_ZERO, NumaNode::NO_NODE)?.cast())
    }
    // SAFETY: not zero sized, by the caller's use
    #[cfg(not(kernel))]
    core::ptr::NonNull::new(unsafe { std::alloc::alloc_zeroed(layout) }).ok_or(AllocError)
}

/// # Safety
/// @p is kzalloc()'s, of @layout, and nothing uses it after.
pub unsafe fn kfree(p: core::ptr::NonNull<u8>, layout: core::alloc::Layout) {
    // SAFETY: the caller's
    #[cfg(kernel)]
    unsafe { <Kmalloc as kernel::alloc::Allocator>::free(p, layout) };
    // SAFETY: the caller's
    #[cfg(not(kernel))]
    unsafe { std::alloc::dealloc(p.as_ptr(), layout) };
}

/// @p - kmalloc()'d, or null - as @new bytes, what was there kept: krealloc(),
/// for a buffer C owns and kfree()s. Of no size the caller knows, as C's
/// aren't. GFP_KERNEL, the caller's memalloc scope saying the rest.
///
/// # Safety
/// @p is null, or kmalloc()'s and not used after unless this fails.
pub unsafe fn krealloc(p: *mut u8, new: usize) -> Result<core::ptr::NonNull<u8>, AllocError> {
    // Kmalloc::realloc() wants the old allocation's layout, which nobody has
    #[cfg(kernel)]
    {
        use kernel::bindings::{krealloc_node_align, GFP_KERNEL, NUMA_NO_NODE};

        // SAFETY: the caller's
        let gfp = current_gfp_context_raw(GFP_KERNEL);
        core::ptr::NonNull::new(unsafe { krealloc_node_align(p.cast(), new, 1, gfp, NUMA_NO_NODE) }.cast())
            .ok_or(AllocError)
    }
    // SAFETY: the caller's
    #[cfg(not(kernel))]
    core::ptr::NonNull::new(unsafe { bcachefs_shim::alloc::krealloc(p, new, crate::c::GFP_KERNEL) }).ok_or(AllocError)
}

/// @size bytes kmalloc()'d, uninitialized: for a buffer of no size anyone
/// keeps - C's kfree(), or c::rust_kfree(), takes it back.
pub fn kmalloc(size: usize) -> Result<core::ptr::NonNull<u8>, AllocError> {
    #[cfg(kernel)]
    {
        use kernel::alloc::{Allocator, NumaNode};

        let layout = core::alloc::Layout::from_size_align(size, 1).map_err(|_| AllocError)?;
        Ok(<Kmalloc as Allocator>::alloc(layout, flags::GFP_KERNEL, NumaNode::NO_NODE)?.cast())
    }
    // SAFETY: a new allocation
    #[cfg(not(kernel))]
    {
        core::ptr::NonNull::new(unsafe { bcachefs_shim::alloc::krealloc(core::ptr::null_mut(), size, crate::c::GFP_KERNEL) })
            .ok_or(AllocError)
    }
}

/// kmalloc()'s alignment of @n bytes: to the largest power of two in it, up
/// to a page - what IO into it, O_DIRECT's in userspace, needs.
fn kv_layout(n: usize) -> Result<core::alloc::Layout, AllocError> {
    let align = if n == 0 { 1 } else { 1 << n.ilog2() };
    let align = align.clamp(size_of::<u64>(), crate::util::kernel::PAGE_SIZE);
    core::alloc::Layout::from_size_align(n, align).map_err(|_| AllocError)
}

/// @size bytes kvmalloc()'d, zeroed: for C's kvfree(), or kvfree_rcu(), to
/// take back.
pub fn kvzalloc(size: usize) -> Result<core::ptr::NonNull<u8>, AllocError> {
    kvrealloc_zeroed(core::ptr::null_mut(), 0, size)
}

/// kvzalloc(), with @gfp: for the rare allocation that needs a flag Flags
/// doesn't name - __GFP_NORETRY. kvfree_sized() takes it back.
pub fn kvzalloc_gfp(size: usize, gfp: crate::c::gfp_t) -> Result<core::ptr::NonNull<u8>, AllocError> {
    let layout = kv_layout(size)?;
    #[cfg(kernel)]
    {
        use kernel::bindings::{kvrealloc_node_align, NUMA_NO_NODE};

        let gfp = current_gfp_context_raw(gfp | crate::c::__GFP_ZERO);
        core::ptr::NonNull::new(unsafe {
            kvrealloc_node_align(core::ptr::null(), size, layout.align() as _, gfp, NUMA_NO_NODE)
        }.cast()).ok_or(AllocError)
    }
    #[cfg(not(kernel))]
    {
        let _ = gfp;
        core::ptr::NonNull::new(unsafe { std::alloc::alloc_zeroed(layout) }).ok_or(AllocError)
    }
}

/// KVmalloc's realloc(), with @gfp: for what needs a flag Flags doesn't name -
/// __GFP_NORETRY.
///
/// # Safety
/// As Alloc::realloc()'s.
pub unsafe fn kvmalloc_realloc_gfp(p: Option<NonNull<u8>>, layout: Layout, old: Layout, gfp: crate::c::gfp_t)
    -> Result<NonNull<u8>, AllocError>
{
    #[cfg(kernel)]
    {
        use kernel::bindings::{kvrealloc_node_align, NUMA_NO_NODE};

        let _ = old;
        let p = p.map_or(core::ptr::null(), |p| p.as_ptr().cast_const().cast());
        NonNull::new(unsafe {
            kvrealloc_node_align(p, layout.size(), layout.align() as _, current_gfp_context_raw(gfp), NUMA_NO_NODE)
        }.cast()).ok_or(AllocError)
    }
    #[cfg(not(kernel))]
    {
        let _ = gfp;
        unsafe { <KVmalloc as Alloc>::realloc(p, layout, old, flags::GFP_KERNEL) }
    }
}

/// @p, @old bytes of kvmalloc()'d memory - or none, null - as @new bytes,
/// what was there kept and the rest zeroed: kvrealloc(), __GFP_ZERO.
/// Aligned as kmalloc() aligns @new. kvfree_sized() takes it back.
pub fn kvrealloc_zeroed(p: *mut u8, old: usize, new: usize) -> Result<core::ptr::NonNull<u8>, AllocError> {
    #[cfg(kernel)]
    {
        use kernel::alloc::{flags::__GFP_ZERO, Allocator, NumaNode};

        // SAFETY: @p is KVmalloc's, of @old bytes, or null
        Ok(unsafe {
            <KVmalloc as Allocator>::realloc(core::ptr::NonNull::new(p), kv_layout(new)?, kv_layout(old)?,
                                             flags::GFP_KERNEL | __GFP_ZERO, NumaNode::NO_NODE)
        }?.cast())
    }
    // A new allocation and a copy: realloc() keeps the old one's alignment
    #[cfg(not(kernel))]
    // SAFETY: @p is std::alloc's, of @old bytes, or null
    unsafe {
        let q = core::ptr::NonNull::new(std::alloc::alloc_zeroed(kv_layout(new)?)).ok_or(AllocError)?;
        if let Some(p) = core::ptr::NonNull::new(p) {
            q.as_ptr().copy_from_nonoverlapping(p.as_ptr(), old.min(new));
            std::alloc::dealloc(p.as_ptr(), kv_layout(old)?);
        }
        Ok(q)
    }
}

/// @p, @old bytes of kvmalloc()'d memory - or none, null - as @new bytes,
/// what was there kept: kvrealloc(). Aligned as kmalloc() aligns @new;
/// kvfree_sized() takes it back.
pub fn kvrealloc(p: *mut u8, old: usize, new: usize) -> Result<core::ptr::NonNull<u8>, AllocError> {
    #[cfg(kernel)]
    {
        use kernel::alloc::{Allocator, NumaNode};

        // SAFETY: @p is KVmalloc's, of @old bytes, or null
        Ok(unsafe {
            <KVmalloc as Allocator>::realloc(core::ptr::NonNull::new(p), kv_layout(new)?, kv_layout(old)?,
                                             flags::GFP_KERNEL, NumaNode::NO_NODE)
        }?.cast())
    }
    // A new allocation and a copy: realloc() keeps the old one's alignment
    #[cfg(not(kernel))]
    // SAFETY: @p is std::alloc's, of @old bytes, or null
    unsafe {
        let q = core::ptr::NonNull::new(std::alloc::alloc(kv_layout(new)?)).ok_or(AllocError)?;
        if let Some(p) = core::ptr::NonNull::new(p) {
            q.as_ptr().copy_from_nonoverlapping(p.as_ptr(), old.min(new));
            std::alloc::dealloc(p.as_ptr(), kv_layout(old)?);
        }
        Ok(q)
    }
}

/// What a kvmalloc() reallocation that mustn't fail falls back to, a plain
/// one having failed: @new bytes, __GFP_NOFAIL - through C, as the kernel's
/// Rust has no __GFP_NOFAIL - @p's @old copied in, and @p freed. Aligned as
/// kmalloc() aligns @new. Userspace's allocator aborts rather than fail.
///
/// # Safety
/// @p is kvmalloc()'d, of @old bytes, and nothing uses it after.
pub unsafe fn kvrealloc_nofail_fallback(p: Option<core::ptr::NonNull<u8>>, old: usize, new: usize)
    -> core::ptr::NonNull<u8>
{
    #[cfg(kernel)]
    {
        let q = kvzalloc_nofail(new);
        if let Some(p) = p {
            unsafe {
                q.as_ptr().copy_from_nonoverlapping(p.as_ptr(), old.min(new));
                <KVmalloc as kernel::alloc::Allocator>::free(p, kv_layout(old).expect("its allocation's"));
            }
        }
        q
    }
    #[cfg(not(kernel))]
    {
        let _ = (p, old);
        std::alloc::handle_alloc_error(kv_layout(new).expect("an allocation's"))
    }
}

/// # Safety
/// @p is kvrealloc_zeroed()'s or kvrealloc()'s, of @size bytes, and nothing
/// uses it after.
pub unsafe fn kvfree_sized(p: core::ptr::NonNull<u8>, size: usize) {
    let layout = kv_layout(size).expect("its allocation's");
    // SAFETY: the caller's
    #[cfg(kernel)]
    unsafe { <KVmalloc as kernel::alloc::Allocator>::free(p, layout) };
    // SAFETY: the caller's
    #[cfg(not(kernel))]
    unsafe { std::alloc::dealloc(p.as_ptr(), layout) };
}

// ── Aligned boxes ────────────────────────────────────────────────────────
//
// ABox<T, A>, and AVec<T, A> to build one: a Box, and a Vec, whose memory is
// aligned to A - ConstAlign<N> - as IO into it needs. In userspace they're
// aligned-vec's; in-kernel, which has no global allocator for the crate's,
// the same API over kvmalloc - the part of it that's used. Memory's as the
// task's memalloc scope allows: no flags.

#[cfg(not(kernel))]
pub use aligned_vec::{ABox, AVec, Alignment, ConstAlign};

#[cfg(kernel)]
pub use abox::{ABox, AVec, Alignment, ConstAlign};

#[cfg(kernel)]
mod abox {
    use super::AllocError;
    use core::alloc::Layout;
    use core::marker::PhantomData;
    use core::ptr::NonNull;
    use super::KVmalloc;
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

/// @init run on @slot: PinInit's __init() - __pinned_init(), before the
/// kernel's pin-init v2 (7.3).
///
/// # Safety
/// As PinInit's: @slot is a T's, and stays put if @init needs it to.
pub unsafe fn init_at<T, E>(init: impl ::pin_init::PinInit<T, E>, slot: *mut T) -> Result<(), E> {
    // SAFETY: the caller's
    #[cfg(bch_pin_init_v2)]
    return unsafe { init.__init(slot) };
    // SAFETY: the caller's
    #[cfg(not(bch_pin_init_v2))]
    return unsafe { init.__pinned_init(slot) };
}

/// An initializer that writes nothing, for a field its owner made already: a
/// write op's bio, a bioset's or its owner's.
///
/// # Safety
/// The field's initialized by the time its struct's initializer runs.
pub unsafe fn untouched<T>() -> impl ::pin_init::Init<T> {
    unsafe { ::pin_init::init_from_closure(|_| Ok(())) }
}

/// @init, failing with @err: an allocation's ENOMEM, as the error that says
/// which it was
pub fn init_or<T, E>(init: impl ::pin_init::PinInit<T, E>, err: crate::errcode::bch_errcode)
    -> impl ::pin_init::PinInit<T, crate::errcode::BchError> {
    unsafe { ::pin_init::pin_init_from_closure(move |slot| init_at(init, slot).map_err(|_| err.into())) }
}

/// A darray C owns, lent to Rust as a KVVec while this lives; given back -
/// its buffer, length and capacity, however the vector grew it - when it
/// drops, even unwinding, so C's darray_exit() frees it as ever. Zero copy:
/// a darray is kvmalloc()'d and kvfree()'d, as a KVVec is in-kernel; in
/// userspace, posix_memalign()'d and free()'d, which the shim's Vec's malloc()
/// allocator reallocs and frees.
pub struct DArrayLoan<'d, T> {
    d: &'d mut crate::types::DArray<T>,
    v: core::mem::ManuallyDrop<KVVec<T>>,
}

impl<'d, T> DArrayLoan<'d, T> {
    /// # Safety
    /// @d is a darray: @nr of its @size initialized, its buffer kvmalloc()'d -
    /// none of C's preallocated storage, which DArray<T> has none of.
    pub unsafe fn new(d: &'d mut crate::types::DArray<T>) -> Self {
        let data = if d.size != 0 { d.data } else { core::ptr::NonNull::dangling().as_ptr() };
        // SAFETY: the caller's; NULL, for an empty darray, made dangling
        let v = unsafe { KVVec::from_raw_parts(data, d.nr, d.size) };
        DArrayLoan { d, v: core::mem::ManuallyDrop::new(v) }
    }
}

impl<T> core::ops::Deref for DArrayLoan<'_, T> {
    type Target = KVVec<T>;

    fn deref(&self) -> &KVVec<T> {
        &self.v
    }
}

impl<T> core::ops::DerefMut for DArrayLoan<'_, T> {
    fn deref_mut(&mut self) -> &mut KVVec<T> {
        &mut self.v
    }
}

impl<T> Drop for DArrayLoan<'_, T> {
    fn drop(&mut self) {
        // SAFETY: taken once, here; nothing reads it after
        let v = unsafe { core::mem::ManuallyDrop::take(&mut self.v) };
        let (data, nr, size) = v.into_raw_parts();
        self.d.data = if size != 0 { data } else { core::ptr::null_mut() };
        self.d.nr   = nr;
        self.d.size = size;
    }
}

/// Free darray @d, left empty: C's darray_exit(), for a darray a Rust type
/// owns and drops.
///
/// # Safety
/// As DArrayLoan::new(): @d is a darray, its buffer kvmalloc()'d.
pub unsafe fn darray_exit<T>(d: &mut crate::types::DArray<T>) {
    // SAFETY: the caller's; the vector it was lent as dropped, freeing the
    // buffer, and an empty one given back
    *unsafe { DArrayLoan::new(d) } = KVVec::new();
}

/// What darray @d holds, now a KVVec's, @d left empty: for a darray C filled
/// for Rust.
///
/// # Safety
/// As DArrayLoan::new().
pub unsafe fn darray_take<T>(d: &mut crate::types::DArray<T>) -> KVVec<T> {
    // SAFETY: the caller's; an empty vector given back for it
    core::mem::replace(&mut *unsafe { DArrayLoan::new(d) }, KVVec::new())
}

/// Insert @v at @idx, moving what's after it up one: neither build's KVVec
/// has insert(), so it's a push and a rotate.
pub fn kvvec_insert<T>(vec: &mut KVVec<T>, idx: usize, v: T) -> Result<(), AllocError> {
    vec.push(v)?;
    vec[idx..].rotate_right(1);
    Ok(())
}
