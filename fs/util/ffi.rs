// SPDX-License-Identifier: GPL-2.0

//! C's buffers, as slices: for entry points C calls with a pointer and a
//! size.
//!
//! Each is unsafe the same way: the pointer has to be what C says it is, for
//! as long as the slice is used - which the entry point's own contract
//! promises, and these only restate.

use core::ffi::c_void;

/// @size bytes at @p - nothing, for none.
///
/// # Safety
/// @p is valid for @size bytes, for 'a.
pub unsafe fn bytes<'a>(p: *const c_void, size: usize) -> &'a [u8] {
    if size == 0 { &[] } else { unsafe { core::slice::from_raw_parts(p as *const u8, size) } }
}

/// As bytes(), None for a NULL @p - C's way of saying there's no value.
///
/// # Safety
/// As bytes().
pub unsafe fn opt_bytes<'a>(p: *const c_void, size: usize) -> Option<&'a [u8]> {
    (!p.is_null()).then(|| unsafe { bytes(p, size) })
}

/// The @size byte buffer at @p, None for a NULL @p - C's way of asking only
/// how big something is.
///
/// # Safety
/// @p is valid for @size bytes, and nothing else uses them, for 'a.
pub unsafe fn opt_bytes_mut<'a>(p: *mut c_void, size: usize) -> Option<&'a mut [u8]> {
    (!p.is_null()).then(|| unsafe { core::slice::from_raw_parts_mut(p as *mut u8, size) })
}
