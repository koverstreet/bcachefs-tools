// SPDX-License-Identifier: GPL-2.0

//! Fallible allocation for code shared by both builds: the kernel's
//! kernel::alloc in-kernel, bcachefs-shim's copy of the same API in userspace.
//! An allocation failure is an AllocError to handle, never an abort.

#[cfg(kernel)]
pub use kernel::alloc::{flags, AllocError, Flags, KBox, KVVec};

#[cfg(not(kernel))]
pub use bcachefs_shim::alloc::{flags, AllocError, Flags, KVVec};
/// In userspace, a Box is a KBox.
#[cfg(not(kernel))]
pub type KBox<T> = std::boxed::Box<T>;

/// A @T for C to hold, built in place by @init in memory that starts zeroed -
/// padding too, as kvzalloc()'s did - and pinned there: a KVBox in-kernel,
/// given up to a raw pointer. free_pinned() takes it back. Fails with @init's
/// error, or the allocation's.
pub fn alloc_pinned<T, E: From<AllocError>>(init: impl ::pin_init::PinInit<T, E>) -> Result<*mut T, E> {
    alloc_in::<false, T, E>(init)
}

/// Drop and free @p.
///
/// # Safety
/// @p is from alloc_pinned(), and nothing uses it after.
pub unsafe fn free_pinned<T>(p: *mut T) {
    unsafe { free_in::<false, T>(p) }
}

/// alloc_pinned(), kmalloc()'d - a KBox in-kernel - for what's freed from
/// any context: kfree() is safe anywhere, and kvfree() isn't - vfree() may
/// sleep, outside interrupt context. kfree_pinned() takes it back.
pub fn kalloc_pinned<T, E: From<AllocError>>(init: impl ::pin_init::PinInit<T, E>) -> Result<*mut T, E> {
    alloc_in::<true, T, E>(init)
}

/// @b's value, out of its box: *b, for a std Box - which the kernel's isn't,
/// so it has into_inner().
pub fn kbox_into_inner<T>(b: KBox<T>) -> T {
    #[cfg(kernel)]
    { KBox::into_inner(b) }
    #[cfg(not(kernel))]
    { *b }
}

pub fn kbox_pin_init<T, E: From<AllocError>>(init: impl ::pin_init::PinInit<T, E>)
    -> Result<core::pin::Pin<KBox<T>>, E>
{
    // SAFETY: kalloc_pinned()'s is a KBox's allocation, pinned
    Ok(unsafe { core::pin::Pin::new_unchecked(KBox::from_raw(kalloc_pinned(init)?)) })
}

/// Drop and free @p - from any context, if dropping it can be.
///
/// # Safety
/// @p is from kalloc_pinned(), and nothing uses it after.
pub unsafe fn kfree_pinned<T>(p: *mut T) {
    unsafe { free_in::<true, T>(p) }
}

/// @layout's worth of memory, zeroed, as a KBox allocates: for a KBox of an
/// unsized type it can't make itself - a struct ending in a slice - by
/// KBox::from_raw(). Or kfree(), if it's never made.
pub fn kzalloc(layout: core::alloc::Layout) -> Result<core::ptr::NonNull<u8>, AllocError> {
    #[cfg(kernel)]
    {
        use kernel::alloc::{allocator::Kmalloc, flags::__GFP_ZERO, Allocator, NumaNode};

        Ok(Kmalloc::alloc(layout, flags::GFP_KERNEL | __GFP_ZERO, NumaNode::NO_NODE)?.cast())
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
    unsafe { <kernel::alloc::allocator::Kmalloc as kernel::alloc::Allocator>::free(p, layout) };
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
        core::ptr::NonNull::new(unsafe { krealloc_node_align(p.cast(), new, 1, GFP_KERNEL, NUMA_NO_NODE) }.cast())
            .ok_or(AllocError)
    }
    // SAFETY: the caller's
    #[cfg(not(kernel))]
    core::ptr::NonNull::new(unsafe { bcachefs_shim::alloc::krealloc(p, new) }).ok_or(AllocError)
}

/// kmalloc()'s alignment of @n bytes: to the largest power of two in it, up
/// to a page - what IO into it, O_DIRECT's in userspace, needs.
fn kv_layout(n: usize) -> Result<core::alloc::Layout, AllocError> {
    let align = if n == 0 { 1 } else { 1 << n.ilog2() };
    let align = align.clamp(size_of::<u64>(), crate::util::kernel::PAGE_SIZE);
    core::alloc::Layout::from_size_align(n, align).map_err(|_| AllocError)
}

/// @p, @old bytes of kvmalloc()'d memory - or none, null - as @new bytes,
/// what was there kept and the rest zeroed: kvrealloc(), __GFP_ZERO.
/// Aligned as kmalloc() aligns @new. kvfree_sized() takes it back.
pub fn kvrealloc_zeroed(p: *mut u8, old: usize, new: usize) -> Result<core::ptr::NonNull<u8>, AllocError> {
    #[cfg(kernel)]
    {
        use kernel::alloc::{allocator::KVmalloc, flags::__GFP_ZERO, Allocator, NumaNode};

        // SAFETY: @p is KVmalloc's, of @old bytes, or null
        Ok(unsafe {
            KVmalloc::realloc(core::ptr::NonNull::new(p), kv_layout(new)?, kv_layout(old)?,
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
/// what was there kept: kvrealloc(). @gfp for where it can't sleep -
/// GFP_NOWAIT, under a spinlock; elsewhere GFP_KERNEL, the caller's memalloc
/// scope saying the rest. Aligned as kmalloc() aligns @new; kvfree_sized()
/// takes it back.
pub fn kvrealloc(p: *mut u8, old: usize, new: usize, gfp: Flags)
    -> Result<core::ptr::NonNull<u8>, AllocError>
{
    #[cfg(kernel)]
    {
        use kernel::alloc::{allocator::KVmalloc, Allocator, NumaNode};

        // SAFETY: @p is KVmalloc's, of @old bytes, or null
        Ok(unsafe {
            KVmalloc::realloc(core::ptr::NonNull::new(p), kv_layout(new)?, kv_layout(old)?,
                              gfp, NumaNode::NO_NODE)
        }?.cast())
    }
    // A new allocation and a copy: realloc() keeps the old one's alignment
    #[cfg(not(kernel))]
    // SAFETY: @p is std::alloc's, of @old bytes, or null
    unsafe {
        let _ = gfp;
        let q = core::ptr::NonNull::new(std::alloc::alloc(kv_layout(new)?)).ok_or(AllocError)?;
        if let Some(p) = core::ptr::NonNull::new(p) {
            q.as_ptr().copy_from_nonoverlapping(p.as_ptr(), old.min(new));
            std::alloc::dealloc(p.as_ptr(), kv_layout(old)?);
        }
        Ok(q)
    }
}

/// # Safety
/// @p is kvrealloc_zeroed()'s or kvrealloc()'s, of @size bytes, and nothing
/// uses it after.
pub unsafe fn kvfree_sized(p: core::ptr::NonNull<u8>, size: usize) {
    let layout = kv_layout(size).expect("its allocation's");
    // SAFETY: the caller's
    #[cfg(kernel)]
    unsafe { <kernel::alloc::allocator::KVmalloc as kernel::alloc::Allocator>::free(p, layout) };
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
    use kernel::alloc::{allocator::KVmalloc, flags::GFP_KERNEL, Allocator, NumaNode};

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

fn alloc_in<const KMALLOC: bool, T, E: From<AllocError>>(init: impl ::pin_init::PinInit<T, E>)
    -> Result<*mut T, E>
{
    #[cfg(kernel)]
    {
        use core::pin::Pin;
        use kernel::alloc::{flags::__GFP_ZERO, KBox, KVBox};
        use kernel::init::InPlaceInit;

        let gfp = flags::GFP_KERNEL | __GFP_ZERO;
        // SAFETY: pinned still: given up to its holder, and back to free_in()
        Ok(unsafe {
            if KMALLOC {
                KBox::into_raw(Pin::into_inner_unchecked(KBox::try_pin_init(init, gfp)?))
            } else {
                KVBox::into_raw(Pin::into_inner_unchecked(KVBox::try_pin_init(init, gfp)?))
            }
        })
    }
    #[cfg(not(kernel))]
    {
        let layout = std::alloc::Layout::new::<T>();
        // SAFETY: not zero sized - what C holds a pointer to has bytes
        let p = unsafe { std::alloc::alloc_zeroed(layout) }.cast::<T>();
        if p.is_null() {
            return Err(AllocError.into());
        }
        // SAFETY: valid for writes, and it won't move: it's C's from here
        match unsafe { init_at(init, p) } {
            Ok(()) => Ok(p),
            Err(e) => {
                // SAFETY: alloc_zeroed()'s, with this layout; a failed init
                // has dropped what it made
                unsafe { std::alloc::dealloc(p.cast(), layout) };
                Err(e)
            }
        }
    }
}

/// # Safety
/// @p is from alloc_in::<KMALLOC>(), and nothing uses it after.
unsafe fn free_in<const KMALLOC: bool, T>(p: *mut T) {
    #[cfg(kernel)]
    unsafe {
        if KMALLOC {
            drop(kernel::alloc::KBox::from_raw(p));
        } else {
            drop(kernel::alloc::KVBox::from_raw(p));
        }
    }
    #[cfg(not(kernel))]
    drop(unsafe { std::boxed::Box::from_raw(p) });
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
    vec.push(v, flags::GFP_KERNEL)?;
    vec[idx..].rotate_right(1);
    Ok(())
}
