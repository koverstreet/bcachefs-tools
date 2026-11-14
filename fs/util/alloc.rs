// SPDX-License-Identifier: GPL-2.0

//! Fallible allocation for code shared by both builds: in-kernel, the
//! kernel's kernel::alloc over allocators of ours, which apply the task's
//! memalloc scope; bcachefs-shim's copy of the same API in userspace. An
//! allocation failure is an AllocError to handle, never an abort.

pub use crate::os::alloc::*;

use core::alloc::Layout;
use core::ptr::NonNull;

use crate::util::maybe_rcu_alloc::MaybeRcuAlloc;
use crate::util::rcu::FreeAnytime;

// ── Allocators ───────────────────────────────────────────────────────────
//
// The kernel's, with the flags the task's memalloc scope says - so KVVec,
// KBox and everything built on them honour NORECLAIM and NOWARN, which the
// page allocator doesn't know. In userspace, std's allocator takes no flags.
// Each port's Alloc impls are its own (os::alloc).

#[derive(Clone, Copy)]
pub struct Kmalloc;
#[derive(Clone, Copy)]
pub struct KVmalloc;

/// How an allocation may fail: as @Flags let it - or, for what mustn't, not
/// at all: a plain GFP_KERNEL allocation first, then __GFP_NOFAIL.
#[derive(Clone, Copy)]
pub enum AllocMode {
    Gfp(Flags),
    Nofail,
}

/// An allocator, as both builds have it: in-kernel, ours above, and in
/// userspace, std's. GFP_KERNEL, as the task's memalloc scope says - NOIO,
/// NOFS, or NORECLAIM, where what would wait fails instead.
///
/// What alloc() gives is at least @layout's bytes, as many as it says: free()
/// is told the size it gave. As Rust's Allocator: alloc() gives them as they
/// are, alloc_zeroed() zeroed.
///
/// # Safety
/// alloc() gives @layout's bytes or more, aligned to it - alloc_zeroed()
/// zeroed; free() frees them - and in-kernel kvfree() too, if KVFREE - from
/// any context. A zero-sized one has no state: stateless() makes it from
/// nothing.
pub unsafe trait Alloc {
    /// Its memory is kvfree()'s to free: so kvfree_rcu()'s, a grace period on.
    const KVFREE: bool = false;

    fn alloc(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError>;

    fn alloc_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError>;

    /// # Safety
    /// @p is alloc()'s, or realloc()'s, @layout's size what it gave, and
    /// nothing uses it after.
    unsafe fn free(&self, p: NonNull<u8>, layout: Layout);
}

/// kmalloc() and kvmalloc(), for what knows its context better than its
/// scope does: GFP_ATOMIC, GFP_NOWAIT.
///
/// # Safety
/// As Alloc's; realloc() gives the new layout's bytes, what was there kept up
/// to the smaller of the two.
pub unsafe trait AllocGfp: Alloc {
    fn alloc_zeroed_gfp(&self, layout: Layout, gfp: Flags) -> Result<NonNull<[u8]>, AllocError>;

    /// @p - None for a new allocation - as @layout, not zeroed: @p freed if
    /// it moves, and kept if this fails.
    ///
    /// # Safety
    /// @p is this allocator's, of @old.
    unsafe fn realloc(&self, p: Option<NonNull<u8>>, layout: Layout, old: Layout, gfp: Flags)
        -> Result<NonNull<[u8]>, AllocError>;

    /// alloc_zeroed(), __GFP_NOFAIL: through C's kzalloc() and kvzalloc(), as
    /// the kernel's Rust has no __GFP_NOFAIL - aligned as kmalloc() aligns, to
    /// 8. Userspace's allocator aborts rather than fail.
    fn alloc_zeroed_nofail(&self, layout: Layout) -> NonNull<[u8]>;

    fn alloc_zeroed_mode(&self, layout: Layout, mode: AllocMode) -> Result<NonNull<[u8]>, AllocError> {
        match mode {
            AllocMode::Gfp(gfp) => self.alloc_zeroed_gfp(layout, gfp),
            AllocMode::Nofail => {
                // checked whether the fallback's needed or not: a misaligned
                // caller would otherwise find out only under memory pressure
                assert!(layout.align() <= 8, "a __GFP_NOFAIL allocation aligned past kmalloc()'s");
                Ok(self.alloc_zeroed(layout).unwrap_or_else(|_| self.alloc_zeroed_nofail(layout)))
            }
        }
    }
}

/// The allocator of what keeps none - Vec, MaybeRcuAlloc: zero-sized, made
/// where it's used. Only those, for now.
pub fn stateless<A: Alloc>() -> A {
    const { assert!(size_of::<A>() == 0, "an allocator with state, where nothing keeps it") };
    unsafe { NonNull::<A>::dangling().read() }
}

/// An allocator by reference: what keeps one keeps &A - a pool's.
unsafe impl<A: Alloc + ?Sized> Alloc for &A {
    const KVFREE: bool = A::KVFREE;

    fn alloc(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        (**self).alloc(layout)
    }

    fn alloc_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        (**self).alloc_zeroed(layout)
    }

    unsafe fn free(&self, p: NonNull<u8>, layout: Layout) {
        unsafe { (**self).free(p, layout) }
    }
}

/// A @T in what @A gave, A kept with it: dropped, and given back to A by the
/// size A gave. A slice's too, new_uninit_slice()'s.
pub struct AllocBox<T: ?Sized, A: Alloc> {
    ptr:   NonNull<T>,
    /// what A gave, which its free goes by
    given: usize,
    alloc: A,
}

unsafe impl<T: ?Sized + Send, A: Alloc + Send> Send for AllocBox<T, A> {}
unsafe impl<T: ?Sized + Sync, A: Alloc + Sync> Sync for AllocBox<T, A> {}

impl<T, A: Alloc> AllocBox<T, A> {
    pub fn new<E: From<AllocError>>(alloc: A, init: impl ::pin_init::Init<T, E>) -> Result<Self, E> {
        let mem = alloc.alloc(Layout::new::<T>())?;
        unsafe { Self::init(alloc, mem, init) }
    }

    /// # Safety
    /// @mem is @alloc's, for a T.
    unsafe fn init<E>(alloc: A, mem: NonNull<[u8]>, init: impl ::pin_init::Init<T, E>) -> Result<Self, E> {
        if let Err(e) = unsafe { init_at(init, mem.cast::<T>().as_ptr()) } {
            unsafe { alloc.free(mem.cast(), Layout::from_size_align_unchecked(mem.len(), align_of::<T>())) };
            return Err(e);
        }
        Ok(AllocBox { ptr: mem.cast(), given: mem.len(), alloc })
    }
}

impl<U: ::pin_init::Zeroable, A: Alloc> AllocBox<[U], A> {
    /// @len Us, zeroed: what a U may be.
    pub fn new_zeroed_slice(alloc: A, len: usize) -> Result<Self, AllocError> {
        let mem = alloc.alloc_zeroed(Layout::array::<U>(len).map_err(|_| AllocError)?)?;
        Ok(AllocBox { ptr: NonNull::slice_from_raw_parts(mem.cast(), len), given: mem.len(), alloc })
    }
}

impl<U, A: Alloc> AllocBox<[core::mem::MaybeUninit<U>], A> {
    /// @len Us, as yet anything.
    pub fn new_uninit_slice(alloc: A, len: usize) -> Result<Self, AllocError> {
        let mem = alloc.alloc(Layout::array::<U>(len).map_err(|_| AllocError)?)?;
        Ok(AllocBox { ptr: NonNull::slice_from_raw_parts(mem.cast(), len), given: mem.len(), alloc })
    }
}

impl<T: ?Sized, A: Alloc> AllocBox<T, A> {
    pub fn as_ptr(&self) -> NonNull<T> {
        self.ptr
    }
}

impl<T: ?Sized, A: Alloc> core::ops::Deref for AllocBox<T, A> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { self.ptr.as_ref() }
    }
}

impl<T: ?Sized, A: Alloc> core::ops::DerefMut for AllocBox<T, A> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { self.ptr.as_mut() }
    }
}

impl<T: ?Sized, A: Alloc> Drop for AllocBox<T, A> {
    fn drop(&mut self) {
        unsafe {
            let align = align_of_val(self.ptr.as_ref());
            core::ptr::drop_in_place(self.ptr.as_ptr());
            self.alloc.free(self.ptr.cast(), Layout::from_size_align_unchecked(self.given, align));
        }
    }
}

pub use crate::data_structures::vec::{DrainAll, IntoIter, PushError, Vec};

pub type KVec<T> = Vec<T, Kmalloc>;
pub type KVVec<T> = Vec<T, KVmalloc>;

/// A @T for C to hold: a MaybeRcuAlloc's, kvmalloc()'d, given up to a raw
/// pointer, which free_pinned() takes back.
pub fn alloc_pinned<T, E: From<AllocError>>(init: impl ::pin_init::PinInit<T, E>) -> Result<*mut T, E> {
    MaybeRcuAlloc::<T, KVmalloc>::new(init).map(|b| b.into_raw().as_ptr())
}

/// alloc_pinned(), the allocation failing with @err - @init with its own.
pub fn alloc_pinned_or<T>(init: impl ::pin_init::PinInit<T, crate::errcode::BchError>, err: crate::errcode::Errcode)
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

/// A KBox frees when it drops, without waiting out RCU readers: what they may
/// reach, which isn't FreeAnytime, can't be in one.
pub fn kbox_pin_init<T: FreeAnytime, E: From<AllocError>>(init: impl ::pin_init::PinInit<T, E>)
    -> Result<core::pin::Pin<KBox<T>>, E>
{
    let p = crate::util::maybe_rcu_alloc::alloc_at::<T, Kmalloc, E>(init, AllocMode::Gfp(flags::GFP_KERNEL))?;
    Ok(unsafe { core::pin::Pin::new_unchecked(KBox::from_raw(p.as_ptr())) })
}

/// kmalloc()'s alignment of @n bytes: to the largest power of two in it, up
/// to a page - what IO into it, O_DIRECT's in userspace, needs.
pub(crate) fn kv_layout(n: usize) -> Result<core::alloc::Layout, AllocError> {
    let align = if n == 0 { 1 } else { 1 << n.ilog2() };
    let align = align.clamp(size_of::<u64>(), crate::os::kernel::PAGE_SIZE);
    core::alloc::Layout::from_size_align(n, align).map_err(|_| AllocError)
}

/// @size bytes kvmalloc()'d, zeroed: for C's kvfree(), or kvfree_rcu(), to
/// take back.
pub fn kvzalloc(size: usize) -> Result<core::ptr::NonNull<u8>, AllocError> {
    kvrealloc_zeroed(core::ptr::null_mut(), 0, size)
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
pub fn init_or<T, E>(init: impl ::pin_init::PinInit<T, E>, err: crate::errcode::Errcode)
    -> impl ::pin_init::PinInit<T, crate::errcode::BchError> {
    unsafe { ::pin_init::pin_init_from_closure(move |slot| init_at(init, slot).map_err(|_| err.into())) }
}

/// Insert @v at @idx, moving what's after it up one: neither build's KVVec
/// has insert(), so it's a push and a rotate.
pub fn kvvec_insert<T>(vec: &mut KVVec<T>, idx: usize, v: T) -> Result<(), AllocError> {
    vec.push(v)?;
    vec[idx..].rotate_right(1);
    Ok(())
}
