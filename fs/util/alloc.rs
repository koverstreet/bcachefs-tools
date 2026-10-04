// SPDX-License-Identifier: GPL-2.0

//! Fallible allocation for code shared by both builds: the kernel's
//! kernel::alloc in-kernel, bcachefs-shim's copy of the same API in userspace.
//! An allocation failure is an AllocError to handle, never an abort.

#[cfg(kernel)]
pub use kernel::alloc::{flags, AllocError, KVVec};

#[cfg(not(kernel))]
pub use bcachefs_shim::alloc::{flags, AllocError, KVVec};
