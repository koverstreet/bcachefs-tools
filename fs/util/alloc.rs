// SPDX-License-Identifier: GPL-2.0

//! Fallible allocation for code shared by both builds: allocators of ours,
//! with os::alloc's Flags - the kernel's C allocators in-kernel, with the
//! task's memalloc scope applied; std's in userspace. An allocation failure is
//! an AllocError to handle, never an abort - unless it's NOFAIL.

pub use crate::os::alloc::*;
pub use core::alloc::{AllocError, Allocator};

use core::alloc::Layout;
use core::mem::MaybeUninit;
use core::ptr::NonNull;

use crate::util::maybe_rcu_alloc::MaybeRcuAlloc;
use crate::util::vstructs::Unsized;
use crate::util::rcu::FreeAnytime;

// ── Allocators ───────────────────────────────────────────────────────────
//
// core's Allocator: in-kernel, the kernel's C allocators with the flags the
// task's memalloc scope says - so Vec, KBox and everything built on them
// honour NORECLAIM and NOWARN, which the page allocator doesn't know, and
// what's vmalloc()'d is freed atomically under NORECLAIM, a spinlock's or an
// RCU read section's, where vfree() can't sleep. In userspace, std's
// allocator. Each port's impls are its own (os::alloc).

#[derive(Clone, Copy, ::pin_init::Zeroable)]
pub struct Kmalloc;
#[derive(Clone, Copy, ::pin_init::Zeroable)]
pub struct KVmalloc;
/// vmalloc(): pages one at a time, mapped - no high-order allocation, to wait on compaction.
#[derive(Clone, Copy, ::pin_init::Zeroable)]
pub struct Vmalloc;

/// An Allocator of ours, which says whether kvfree() frees what it gives.
///
/// # Safety
/// If KVFREE, kvfree() - in-kernel - frees what it gives: as kvfree_rcu()
/// does, from a work item or an RCU callback. A zero-sized one has no state:
/// stateless() makes it from nothing.
pub unsafe trait Alloc: Allocator {
    /// Its memory is kvfree()'s to free: so kvfree_rcu()'s, a grace period on.
    const KVFREE: bool = false;
}

/// An allocator by reference: what keeps one keeps &A - a pool's.
unsafe impl<A: Alloc + ?Sized> Alloc for &A {
    const KVFREE: bool = A::KVFREE;
}

/// Allocator's allocate() and grow(), with flags: for what knows its context
/// better than its scope does - ATOMIC, NOWAIT - or mustn't fail: NOFAIL.
///
/// # Safety
/// As Allocator's.
pub unsafe trait AllocGfp: Alloc {
    fn allocate_gfp(&self, layout: Layout, gfp: Flags) -> Result<NonNull<[u8]>, AllocError>;

    fn allocate_zeroed_gfp(&self, layout: Layout, gfp: Flags) -> Result<NonNull<[u8]>, AllocError> {
        self.allocate_gfp(layout, gfp | flags::ZERO)
    }

    /// # Safety
    /// As Allocator::grow()'s.
    unsafe fn grow_gfp(&self, p: NonNull<u8>, old: Layout, new: Layout, gfp: Flags)
        -> Result<NonNull<[u8]>, AllocError>;

    /// This allocator, its allocations with @gfp: how what takes an
    /// allocator - a Box - allocates specially.
    fn with_flags(self, gfp: Flags) -> WithFlags<Self>
    where
        Self: Sized,
    {
        WithFlags(self, gfp)
    }
}

/// AllocGfp::with_flags()'s.
#[derive(Clone, Copy)]
pub struct WithFlags<A>(A, Flags);

unsafe impl<A: AllocGfp> Allocator for WithFlags<A> {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        self.0.allocate_gfp(layout, self.1)
    }

    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        self.0.allocate_zeroed_gfp(layout, self.1)
    }

    unsafe fn deallocate(&self, p: NonNull<u8>, layout: Layout) {
        unsafe { self.0.deallocate(p, layout) }
    }

    unsafe fn grow(&self, p: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
        unsafe { self.0.grow_gfp(p, old, new, self.1) }
    }
}

unsafe impl<A: AllocGfp> Alloc for WithFlags<A> {
    const KVFREE: bool = A::KVFREE;
}

/// Its flags, and what's asked
unsafe impl<A: AllocGfp> AllocGfp for WithFlags<A> {
    fn allocate_gfp(&self, layout: Layout, gfp: Flags) -> Result<NonNull<[u8]>, AllocError> {
        self.0.allocate_gfp(layout, gfp | self.1)
    }

    unsafe fn grow_gfp(&self, p: NonNull<u8>, old: Layout, new: Layout, gfp: Flags)
        -> Result<NonNull<[u8]>, AllocError>
    {
        unsafe { self.0.grow_gfp(p, old, new, gfp | self.1) }
    }
}

/// An allocator by reference, with flags: a mempool's.
unsafe impl<A: AllocGfp + ?Sized> AllocGfp for &A {
    fn allocate_gfp(&self, layout: Layout, gfp: Flags) -> Result<NonNull<[u8]>, AllocError> {
        (**self).allocate_gfp(layout, gfp)
    }

    fn allocate_zeroed_gfp(&self, layout: Layout, gfp: Flags) -> Result<NonNull<[u8]>, AllocError> {
        (**self).allocate_zeroed_gfp(layout, gfp)
    }

    unsafe fn grow_gfp(&self, p: NonNull<u8>, old: Layout, new: Layout, gfp: Flags)
        -> Result<NonNull<[u8]>, AllocError>
    {
        unsafe { (**self).grow_gfp(p, old, new, gfp) }
    }
}

/// The allocator of what keeps none - Vec, MaybeRcuAlloc: zero-sized, made
/// where it's used. Only those, for now.
pub fn stateless<A: Alloc>() -> A {
    const { assert!(size_of::<A>() == 0, "an allocator with state, where nothing keeps it") };
    try_stateless().expect("zero-sized")
}

/// stateless(), or None for an allocator with state: for a path a build
/// makes whether or not it's taken - a drop an unwind may run - where an
/// allocator with state is a bug only once it's reached.
pub fn try_stateless<A: Alloc>() -> Option<A> {
    (size_of::<A>() == 0).then(|| unsafe { NonNull::<A>::dangling().read() })
}

/// std's Box<T, A>, as much of it as is used, in both builds: the kernel has
/// no alloc crate. Unlike std's, it frees by the size A gave, not T's - a
/// mempool tells its elements by their size.
pub struct Box<T: ?Sized, A: Alloc = crate::os::alloc::Global> {
    ptr:   NonNull<T>,
    /// what A gave, which its free goes by
    given: usize,
    alloc: A,
}

unsafe impl<T: ?Sized + Send, A: Alloc + Send> Send for Box<T, A> {}
unsafe impl<T: ?Sized + Sync, A: Alloc + Sync> Sync for Box<T, A> {}

// None is its pointer null - zeroed - whatever the rest are
unsafe impl<T: ?Sized, A: Alloc> ::pin_init::ZeroableOption for Box<T, A> {}

impl<T, A: Alloc> Box<T, A> {
    /// @init's T, in place: pin-init's try_init(), in @alloc.
    pub fn try_init_in<E: From<AllocError>>(init: impl ::pin_init::Init<T, E>, alloc: A) -> Result<Self, E> {
        let mem = alloc.allocate(Layout::new::<T>())?;
        if let Err(e) = unsafe { init_at(init, mem.cast::<T>().as_ptr()) } {
            unsafe { alloc.deallocate(mem.cast(), Layout::from_size_align_unchecked(mem.len(), align_of::<T>())) };
            return Err(e);
        }
        Ok(Box { ptr: mem.cast(), given: mem.len(), alloc })
    }

    /// try_init_in(), of an @init that can't fail: pin-init's init().
    pub fn init_in(init: impl ::pin_init::Init<T>, alloc: A) -> Result<Self, AllocError> {
        let mem = alloc.allocate(Layout::new::<T>())?;
        let Ok(()) = unsafe { init_at(init, mem.cast::<T>().as_ptr()) };
        Ok(Box { ptr: mem.cast(), given: mem.len(), alloc })
    }

    /// Its pointer, and its allocator, for from_raw_in() to make it again.
    pub fn into_raw_with_allocator(b: Self) -> (*mut T, A) {
        let b = core::mem::ManuallyDrop::new(b);
        (b.ptr.as_ptr(), unsafe { core::ptr::read(&b.alloc) })
    }

    /// # Safety
    /// @p is into_raw_with_allocator()'s, of an allocator that gave a T's
    /// size - or @alloc's own allocation of a T, of a T's layout - and nothing
    /// else owns it.
    pub unsafe fn from_raw_in(p: *mut T, alloc: A) -> Self {
        Box { ptr: unsafe { NonNull::new_unchecked(p) }, given: size_of::<T>(), alloc }
    }
}

impl<U, A: Alloc> Box<[U], A> {
    pub fn try_new_uninit_slice_in(len: usize, alloc: A) -> Result<Box<[MaybeUninit<U>], A>, AllocError> {
        let mem = alloc.allocate(Layout::array::<U>(len).map_err(|_| AllocError)?)?;
        Ok(Box { ptr: NonNull::slice_from_raw_parts(mem.cast(), len), given: mem.len(), alloc })
    }

    pub fn try_new_zeroed_slice_in(len: usize, alloc: A) -> Result<Box<[MaybeUninit<U>], A>, AllocError> {
        let mem = alloc.allocate_zeroed(Layout::array::<U>(len).map_err(|_| AllocError)?)?;
        Ok(Box { ptr: NonNull::slice_from_raw_parts(mem.cast(), len), given: mem.len(), alloc })
    }

    /// The same allocation, as a T - a DST over these Us - by @cast of its
    /// pointer, which keeps its metadata.
    ///
    /// # Safety
    /// What @cast makes is a valid T, of these bytes.
    pub unsafe fn cast_unsized<T: ?Sized>(b: Self, cast: impl FnOnce(*mut [U]) -> *mut T) -> Box<T, A> {
        let b = core::mem::ManuallyDrop::new(b);
        let ptr = unsafe { NonNull::new_unchecked(cast(b.ptr.as_ptr())) };
        Box { ptr, given: b.given, alloc: unsafe { core::ptr::read(&b.alloc) } }
    }
}

impl<T: ?Sized + Unsized, A: Alloc> Box<T, A> {
    /// A copy of @v - every byte of which is a field's - from @alloc:
    /// Box::clone_from_ref(), for a DST of ours.
    pub fn try_clone_from_ref_in(v: &T, alloc: A) -> Result<Self, AllocError> {
        let len = size_of_val(v);
        let mem = alloc.allocate(Layout::for_value(v))?;
        unsafe { core::ptr::copy_nonoverlapping(core::ptr::from_ref(v).cast::<u8>(), mem.cast::<u8>().as_ptr(), len) };
        let ptr = NonNull::new(T::from_raw_mut(mem.cast().as_ptr(), len)).expect("an allocation, not null");
        Ok(Box { ptr, given: mem.len(), alloc })
    }
}

impl<U: ::pin_init::Zeroable, A: Alloc> Box<[U], A> {
    /// @len Us, zeroed - each a valid U.
    pub fn try_zeroed_slice_in(len: usize, alloc: A) -> Result<Self, AllocError> {
        // SAFETY: zeroed, a U is valid
        Ok(unsafe { Self::try_new_zeroed_slice_in(len, alloc)?.assume_init() })
    }
}

impl<U, A: Alloc> Box<[MaybeUninit<U>], A> {
    /// # Safety
    /// Every element is a valid U.
    pub unsafe fn assume_init(self) -> Box<[U], A> {
        let (ptr, given, alloc) = {
            let b = core::mem::ManuallyDrop::new(self);
            (b.ptr, b.given, unsafe { core::ptr::read(&b.alloc) })
        };
        Box { ptr: NonNull::slice_from_raw_parts(ptr.cast(), ptr.len()), given, alloc }
    }
}

impl<T: ?Sized, A: AllocGfp> Box<T, WithFlags<A>> {
    /// Its allocator without the flags, which freeing it doesn't use.
    pub fn into_unflagged(b: Self) -> Box<T, A> {
        let b = core::mem::ManuallyDrop::new(b);
        Box { ptr: b.ptr, given: b.given, alloc: unsafe { core::ptr::read(&b.alloc.0) } }
    }
}

impl<T: ?Sized, A: Alloc> Box<T, A> {
    pub fn as_ptr(b: &Self) -> *const T {
        b.ptr.as_ptr()
    }

    pub fn as_mut_ptr(b: &mut Self) -> *mut T {
        b.ptr.as_ptr()
    }

    pub fn as_non_null(b: &mut Self) -> NonNull<T> {
        b.ptr
    }
}

impl<T: ?Sized, A: Alloc + 'static> Box<T, A> {
    pub fn into_pin(b: Self) -> core::pin::Pin<Self> {
        unsafe { core::pin::Pin::new_unchecked(b) }
    }
}

impl<T: ?Sized, A: Alloc> core::ops::Deref for Box<T, A> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { self.ptr.as_ref() }
    }
}

impl<T: ?Sized, A: Alloc> core::ops::DerefMut for Box<T, A> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { self.ptr.as_mut() }
    }
}

impl<T: ?Sized, A: Alloc> Drop for Box<T, A> {
    fn drop(&mut self) {
        unsafe {
            let align = align_of_val(self.ptr.as_ref());
            core::ptr::drop_in_place(self.ptr.as_ptr());
            // given nothing - an empty Vec's - it has no allocation
            if self.given != 0 {
                self.alloc.deallocate(self.ptr.cast(), Layout::from_size_align_unchecked(self.given, align));
            }
        }
    }
}

/// std's: the Vec's allocation, as it is - the Box frees by its capacity.
impl<T, A: AllocGfp, L: LenUint> From<Vec<T, A, L>> for Box<[T], A> {
    fn from(v: Vec<T, A, L>) -> Self {
        let (p, len, cap) = v.into_raw_parts();
        let ptr = NonNull::slice_from_raw_parts(NonNull::new(p).expect("a vec's pointer, dangling if none"), len);
        Box { ptr, given: cap * size_of::<T>(), alloc: stateless() }
    }
}

/// A Box aligned to @ALIGN, whatever T's alignment - and freed so: kvmalloc()'d.
pub struct AlignedBox<T: ?Sized, const ALIGN: usize> {
    ptr: NonNull<T>,
}

unsafe impl<T: ?Sized + Send, const ALIGN: usize> Send for AlignedBox<T, ALIGN> {}
unsafe impl<T: ?Sized + Sync, const ALIGN: usize> Sync for AlignedBox<T, ALIGN> {}

// None is its pointer null - zeroed
unsafe impl<T: ?Sized, const ALIGN: usize> ::pin_init::ZeroableOption for AlignedBox<T, ALIGN> {}

impl<U, const ALIGN: usize> AlignedBox<[MaybeUninit<U>], ALIGN> {
    pub fn try_new_uninit_slice_gfp(len: usize, gfp: Flags) -> Result<Self, AllocError> {
        let layout = Layout::array::<U>(len).and_then(|l| l.align_to(ALIGN)).map_err(|_| AllocError)?;
        let mem = KVmalloc.allocate_gfp(layout, gfp)?;
        Ok(AlignedBox { ptr: NonNull::slice_from_raw_parts(mem.cast(), len) })
    }

    /// # Safety
    /// Every element is a valid U.
    unsafe fn assume_init(self) -> AlignedBox<[U], ALIGN> {
        let ptr = core::mem::ManuallyDrop::new(self).ptr;
        AlignedBox { ptr: NonNull::slice_from_raw_parts(ptr.cast(), ptr.len()) }
    }
}

impl<U: ::pin_init::Zeroable, const ALIGN: usize> AlignedBox<[U], ALIGN> {
    pub fn try_new_zeroed_slice_gfp(len: usize, gfp: Flags) -> Result<Self, AllocError> {
        let b = AlignedBox::<[MaybeUninit<U>], ALIGN>::try_new_uninit_slice_gfp(len, gfp | flags::ZERO)?;
        Ok(unsafe { b.assume_init() })
    }
}

impl<T: ?Sized, const ALIGN: usize> core::ops::Deref for AlignedBox<T, ALIGN> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { self.ptr.as_ref() }
    }
}

impl<T: ?Sized, const ALIGN: usize> core::ops::DerefMut for AlignedBox<T, ALIGN> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { self.ptr.as_mut() }
    }
}

impl<T: ?Sized, const ALIGN: usize> Drop for AlignedBox<T, ALIGN> {
    fn drop(&mut self) {
        let layout = Layout::for_value(&**self).align_to(ALIGN).expect("its allocation's layout");
        unsafe {
            core::ptr::drop_in_place(self.ptr.as_ptr());
            // a zero-sized one is dangling: no allocation
            if layout.size() != 0 {
                KVmalloc.deallocate(self.ptr.cast(), layout);
            }
        }
    }
}

pub use crate::data_structures::vec::{DrainAll, IntoIter, PushError, Vec};
use crate::data_structures::array_vec::LenUint;

/// A @T, kvmalloc()'d, the allocation failing with @err - @init with its own.
pub fn alloc_pinned_or<T>(init: impl ::pin_init::PinInit<T, crate::errcode::BchError>, err: crate::errcode::Errcode)
    -> Result<MaybeRcuAlloc<T, KVmalloc>, crate::errcode::BchError>
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
    MaybeRcuAlloc::new(init).map_err(|e| match e {
        Failed::Alloc   => err.into(),
        Failed::Init(e) => e,
    })
}

/// A KBox frees when it drops, without waiting out RCU readers: what they may
/// reach, which isn't FreeAnytime, can't be in one.
pub fn kbox_pin_init<T: FreeAnytime, E: From<AllocError>>(init: impl ::pin_init::PinInit<T, E>)
    -> Result<core::pin::Pin<KBox<T>>, E>
{
    let p = crate::util::maybe_rcu_alloc::alloc_at::<T, Kmalloc, E>(init, Flags::empty())?;
    Ok(unsafe { core::pin::Pin::new_unchecked(KBox::from_raw(p.as_ptr())) })
}

/// kmalloc()'s alignment of @n bytes: to the largest power of two in it, up
/// to a page - what IO into it, O_DIRECT's in userspace, needs.
pub(crate) fn kv_layout(n: usize) -> Result<core::alloc::Layout, AllocError> {
    let align = if n == 0 { 1 } else { 1 << n.ilog2() };
    let align = align.clamp(size_of::<u64>(), crate::os::kernel::PAGE_SIZE);
    core::alloc::Layout::from_size_align(n, align).map_err(|_| AllocError)
}

/// @size bytes kvmalloc()'d, zeroed: for kvfree(), or kvfree_rcu(), to take
/// back.
pub fn kvzalloc(size: usize) -> Result<core::ptr::NonNull<u8>, AllocError> {
    kvrealloc_zeroed(core::ptr::null_mut(), 0, size)
}

/// @init run on @slot: PinInit's __init().
///
/// # Safety
/// As PinInit's: @slot is a T's, and stays put if @init needs it to.
pub unsafe fn init_at<T, E>(init: impl ::pin_init::PinInit<T, E>, slot: *mut T) -> Result<(), E> {
    unsafe { init.__init(slot) }
}

/// An initializer that writes nothing, for a field its owner made already: a
/// write op's bio, a bioset's or its owner's.
///
/// # Safety
/// The field's initialized by the time its struct's initializer runs.
pub unsafe fn untouched<T>() -> impl ::pin_init::Init<T> {
    unsafe { ::pin_init::init_from_closure(|_| Ok(())) }
}

/// What @make returns, written where it's initialized: for what's too big to
/// make and move, a future.
pub fn init_with<T>(make: impl FnOnce() -> T) -> impl ::pin_init::Init<T> {
    unsafe {
        ::pin_init::init_from_closure(move |slot: *mut T| {
            slot.write(make());
            Ok(())
        })
    }
}

/// @init, failing with @err: an allocation's ENOMEM, as the error that says
/// which it was
pub fn init_or<T, E>(init: impl ::pin_init::PinInit<T, E>, err: crate::errcode::Errcode)
    -> impl ::pin_init::PinInit<T, crate::errcode::BchError> {
    unsafe { ::pin_init::pin_init_from_closure(move |slot| init_at(init, slot).map_err(|_| err.into())) }
}
