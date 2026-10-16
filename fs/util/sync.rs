// SPDX-License-Identifier: GPL-2.0

//! Locks for code shared by both builds: the kernel's kernel::sync in-kernel,
//! bcachefs-shim's copy of the same API in userspace - pinned, initialized in
//! place: new_mutex!() is a pin-init initializer, a field's #[init(...)].

#[cfg(kernel)]
pub use kernel::sync::{new_mutex, Mutex};

#[cfg(not(kernel))]
pub use bcachefs_shim::sync::{new_mutex, Mutex};
