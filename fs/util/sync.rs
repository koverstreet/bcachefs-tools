// SPDX-License-Identifier: GPL-2.0

//! Locks for code shared by both builds: the kernel's kernel::sync in-kernel,
//! bcachefs-shim's copy of the same API in userspace - pinned, initialized in
//! place from a PinInit, so a Rust member of a struct C allocates is set up
//! the same way in both (util::ffi::init_in_place()).

#[cfg(kernel)]
pub use kernel::{prelude::PinInit, sync::new_mutex, sync::Mutex};

#[cfg(not(kernel))]
pub use bcachefs_shim::sync::{new_mutex, Mutex, PinInit};
