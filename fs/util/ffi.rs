// SPDX-License-Identifier: GPL-2.0

//! For entry points C calls: C's objects, as references (Opaque), and C's
//! buffers, as slices. (Names are os_str.rs's.)
//!
//! The buffer functions are unsafe the same way: the pointer has to be what
//! C says it is, for as long as the slice is used - which the entry point's
//! own contract promises, and these only restate.

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::marker::PhantomPinned;

/// A C object - a transaction, an iterator, the filesystem - as an entry
/// point's argument: `&Opaque<c::btree_trans>` where C passes a
/// `struct btree_trans *`. Same ABI; transparent.
///
/// Not a plain reference to the C struct, because Rust promises things about
/// those that aren't true of these. `&T` says the memory doesn't change for
/// the reference's life, and `&mut T` that nothing else touches it - LLVM's
/// readonly and noalias. A transaction's locking state is read by other
/// threads' deadlock detection while we run, and the filesystem is shared and
/// changing everywhere. UnsafeCell takes away the first promise, PhantomPinned
/// the second - as the kernel crate's Opaque<T>, which userspace doesn't have.
///
/// There's no constructor: only C hands these out, so one always refers to a
/// live, initialized object of its type - what the safe from_c() constructors
/// (Fs, BtreeTrans, BtreeIter) rely on. Safe code can't make one from a C
/// struct with made-up fields.
#[repr(transparent)]
pub struct Opaque<T> {
    value: UnsafeCell<T>,
    _pin:  PhantomPinned,
}

impl<T> Opaque<T> {
    /// The C object, for C.
    pub fn as_ptr(&self) -> *mut T {
        self.value.get()
    }
}

/// The C string at @p, None for a NULL @p.
///
/// # Safety
/// @p NULL or a NUL-terminated string, valid for 'a.
pub unsafe fn opt_cstr<'a>(p: *const core::ffi::c_char) -> Option<&'a core::ffi::CStr> {
    (!p.is_null()).then(|| unsafe { core::ffi::CStr::from_ptr(p) })
}

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
