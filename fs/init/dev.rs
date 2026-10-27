// SPDX-License-Identifier: GPL-2.0

//! Devices: dev.c's, as it converts.
//!
//! So far a bch_dev's memory. Rust allocates it, as bch_fs's, so its
//! Rust-typed fields have initializers - the bucket bitmaps' Locks - and it's
//! dropped whole when it's freed: after C's teardown, which leaves what it
//! freed nulled for those fields' Drops to find.

use crate::c;

/// The memory of a new bch_dev of @c: bch_dev::init()'s - its Rust-typed
/// fields' initializers run, everything else zeroed - for __bch2_dev_alloc()
/// to go on with. NULL if out of memory.
#[cstruct_macros::c_export]
pub fn bch2_dev_alloc_mem(c: &crate::util::ffi::Opaque<c::bch_fs>) -> *mut c::bch_dev {
    crate::util::alloc::alloc_pinned(c::bch_dev::init(crate::fs::Fs::from_c(c)))
        .unwrap_or(core::ptr::null_mut())
}

/// Drop @ca and free it, from its kobject's release.
///
/// # Safety
/// @ca is from bch2_dev_alloc_mem(), and nothing uses it after.
#[cstruct_macros::c_export]
pub unsafe fn bch2_dev_free_mem(ca: *mut c::bch_dev) {
    unsafe { crate::util::alloc::free_pinned(ca) }
}
