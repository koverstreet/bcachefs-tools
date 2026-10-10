// SPDX-License-Identifier: GPL-2.0

//! Names - filenames, xattr names: bytes, not UTF-8. That's std's OsStr, on
//! unix; userspace uses it, and the kernel build, which is no_std, gets a
//! copy of the part of its interface we use.
//!
//! Written against std's: `OsStr::from_bytes()` and `.as_bytes()` with
//! OsStrExt in scope, the same in both builds.
//!
//! C's struct qstr is the same thing, with no lifetime: qstr() and
//! qstr_name() convert at the boundary.

use crate::c;

#[cfg(feature = "std")]
pub use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

#[cfg(not(feature = "std"))]
pub use no_std::{OsStr, OsStrExt};

#[cfg(not(feature = "std"))]
mod no_std {
    use core::fmt;

    /// std::ffi::OsStr, as on unix: any bytes.
    #[repr(transparent)]
    #[derive(PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct OsStr([u8]);

    impl OsStr {
        pub fn len(&self) -> usize {
            self.0.len()
        }

        pub fn is_empty(&self) -> bool {
            self.0.is_empty()
        }
    }

    /// std::os::unix::ffi::OsStrExt.
    pub trait OsStrExt {
        fn from_bytes(slice: &[u8]) -> &Self;
        fn as_bytes(&self) -> &[u8];
    }

    impl OsStrExt for OsStr {
        fn from_bytes(slice: &[u8]) -> &OsStr {
            // repr(transparent): an OsStr is its bytes
            unsafe { &*(slice as *const [u8] as *const OsStr) }
        }

        fn as_bytes(&self) -> &[u8] {
            &self.0
        }
    }

    /// As std's: the bytes, escaped where they aren't printable ASCII.
    impl fmt::Debug for OsStr {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "\"{}\"", self.0.escape_ascii())
        }
    }
}

/// @name as C's qstr, which points at it: for a C call, which mustn't keep
/// it.
pub fn qstr(name: &OsStr) -> c::qstr {
    let name = name.as_bytes();
    c::qstr {
        __bindgen_anon_1: c::qstr__bindgen_ty_1 {
            __bindgen_anon_1: c::qstr__bindgen_ty_1__bindgen_ty_1 {
                hash: 0,
                len:  name.len() as u32,
            },
        },
        name: name.as_ptr(),
    }
}

/// The NUL-terminated name at @p, without the NUL.
///
/// # Safety
/// @p is a NUL-terminated string, valid for 'a.
pub unsafe fn cstr_name<'a>(p: *const crate::util::ffi::c_char) -> &'a OsStr {
    OsStr::from_bytes(unsafe { core::ffi::CStr::from_ptr(p.cast()) }.to_bytes())
}

/// The name C's qstr @q points at.
///
/// # Safety
/// @q points at its len bytes, and they outlive 'a.
pub unsafe fn qstr_name<'a>(q: &c::qstr) -> &'a OsStr {
    let len = unsafe { q.__bindgen_anon_1.__bindgen_anon_1.len } as usize;
    let bytes = if len == 0 { &[] } else { unsafe { core::slice::from_raw_parts(q.name, len) } };
    OsStr::from_bytes(bytes)
}
