// SPDX-License-Identifier: GPL-2.0

//! Fallible allocation for code shared by both builds: the kernel's
//! kernel::alloc in-kernel, bcachefs-shim's copy of the same API in userspace.
//! An allocation failure is an AllocError to handle, never an abort.

#[cfg(kernel)]
pub use kernel::alloc::{flags, AllocError, KVVec};

#[cfg(not(kernel))]
pub use bcachefs_shim::alloc::{flags, AllocError, KVVec};

/// Insert @v at @idx, moving what's after it up one: neither build's KVVec
/// has insert(), so it's a push and a rotate.
pub fn kvvec_insert<T>(vec: &mut KVVec<T>, idx: usize, v: T) -> Result<(), AllocError> {
    vec.push(v, flags::GFP_KERNEL)?;
    vec[idx..].rotate_right(1);
    Ok(())
}
