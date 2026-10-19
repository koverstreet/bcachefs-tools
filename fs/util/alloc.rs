// SPDX-License-Identifier: GPL-2.0

//! Fallible allocation for code shared by both builds: the kernel's
//! kernel::alloc in-kernel, bcachefs-shim's copy of the same API in userspace.
//! An allocation failure is an AllocError to handle, never an abort.

#[cfg(kernel)]
pub use kernel::alloc::{flags, AllocError, Flags, KVVec};

#[cfg(not(kernel))]
pub use bcachefs_shim::alloc::{flags, AllocError, Flags, KVVec};

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

/// Drop and free @p - from any context, if dropping it can be.
///
/// # Safety
/// @p is from kalloc_pinned(), and nothing uses it after.
pub unsafe fn kfree_pinned<T>(p: *mut T) {
    unsafe { free_in::<true, T>(p) }
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
        match unsafe { init.__pinned_init(p) } {
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

/// Insert @v at @idx, moving what's after it up one: neither build's KVVec
/// has insert(), so it's a push and a rotate.
pub fn kvvec_insert<T>(vec: &mut KVVec<T>, idx: usize, v: T) -> Result<(), AllocError> {
    vec.push(v, flags::GFP_KERNEL)?;
    vec[idx..].rotate_right(1);
    Ok(())
}
