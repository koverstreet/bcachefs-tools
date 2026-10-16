// SPDX-License-Identifier: GPL-2.0

//! Fallible allocation for code shared by both builds: the kernel's
//! kernel::alloc in-kernel, bcachefs-shim's copy of the same API in userspace.
//! An allocation failure is an AllocError to handle, never an abort.

#[cfg(kernel)]
pub use kernel::alloc::{flags, AllocError, KVVec};

#[cfg(not(kernel))]
pub use bcachefs_shim::alloc::{flags, AllocError, KVVec};

/// A @T for C to hold, built in place by @init in memory that starts zeroed -
/// padding too, as kvzalloc()'s did - and pinned there: a KVBox in-kernel,
/// given up to a raw pointer. free_pinned() takes it back.
pub fn alloc_pinned<T>(init: impl ::pin_init::PinInit<T>) -> Result<*mut T, AllocError> {
    #[cfg(kernel)]
    {
        use kernel::alloc::{flags::__GFP_ZERO, KVBox};
        use kernel::init::InPlaceInit;

        // Its init can't fail: only the allocation
        let b = KVBox::pin_init(init, flags::GFP_KERNEL | __GFP_ZERO).map_err(|_| AllocError)?;
        // SAFETY: pinned still - C has the pointer, and gives it back to free_pinned()
        Ok(KVBox::into_raw(unsafe { core::pin::Pin::into_inner_unchecked(b) }))
    }
    #[cfg(not(kernel))]
    {
        let layout = std::alloc::Layout::new::<T>();
        // SAFETY: not zero sized - what C holds a pointer to has bytes
        let p = unsafe { std::alloc::alloc_zeroed(layout) }.cast::<T>();
        if p.is_null() {
            return Err(AllocError);
        }
        // SAFETY: valid for writes, and it won't move: it's C's from here
        match unsafe { init.__pinned_init(p) } {
            Ok(()) => Ok(p),
            Err(e) => match e {},
        }
    }
}

/// Drop and free @p.
///
/// # Safety
/// @p is from alloc_pinned(), and nothing uses it after.
pub unsafe fn free_pinned<T>(p: *mut T) {
    #[cfg(kernel)]
    drop(unsafe { kernel::alloc::KVBox::from_raw(p) });
    #[cfg(not(kernel))]
    drop(unsafe { std::boxed::Box::from_raw(p) });
}

/// Insert @v at @idx, moving what's after it up one: neither build's KVVec
/// has insert(), so it's a push and a rotate.
pub fn kvvec_insert<T>(vec: &mut KVVec<T>, idx: usize, v: T) -> Result<(), AllocError> {
    vec.push(v, flags::GFP_KERNEL)?;
    vec[idx..].rotate_right(1);
    Ok(())
}
