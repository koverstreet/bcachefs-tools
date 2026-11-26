// SPDX-License-Identifier: GPL-2.0
//! Raw bindings for the userspace Linux kernel-compat layer — the in-tree
//! `include/` shim that bcachefs's fs/ code is built against in userspace.
//!
//! This is the userspace stand-in for the in-kernel `kernel` crate: the fs/
//! bindings (the `bcachefs-kernel` crate) own only bcachefs's own types, and
//! import everything else — `bio`, the `__le*`/`__u*` primitives, locks,
//! lists — from here. In a real kernel build those come from the kernel
//! itself, and this crate is swapped out behind a cfg.

pub mod sync;

/// What the vendored aref.rs implements for ARef: the kernel's types.rs, the
/// rest of which needs the kernel's proc macros
pub mod types {
    use core::ffi::c_void;

    /// kernel::types::ForeignOwnable: a type owned through a raw pointer.
    ///
    /// # Safety
    /// As the kernel's: into_foreign()'s pointer is non-null and
    /// FOREIGN_ALIGN aligned.
    pub unsafe trait ForeignOwnable: Sized {
        const FOREIGN_ALIGN: usize;

        type Borrowed<'a>
        where
            Self: 'a;
        type BorrowedMut<'a>
        where
            Self: 'a;

        fn into_foreign(self) -> *mut c_void;

        /// # Safety
        /// @ptr is into_foreign()'s, not yet taken back.
        unsafe fn from_foreign(ptr: *mut c_void) -> Self;

        /// # Safety
        /// @ptr is null, or as from_foreign()'s.
        unsafe fn try_from_foreign(ptr: *mut c_void) -> Option<Self> {
            (!ptr.is_null()).then(|| unsafe { Self::from_foreign(ptr) })
        }

        /// # Safety
        /// @ptr is into_foreign()'s, and outlives 'a.
        unsafe fn borrow<'a>(ptr: *mut c_void) -> Self::Borrowed<'a>;

        /// # Safety
        /// As borrow()'s, and nothing else borrows it for 'a.
        unsafe fn borrow_mut<'a>(ptr: *mut c_void) -> Self::BorrowedMut<'a>;
    }
}

/// C's types, as the kernel crate's ffi maps them: long is isize, as intptr_t
/// is on every Linux target. char is the platform's here, as userspace's
/// libraries are compiled with it; the kernel's is u8, for -funsigned-char.
#[allow(non_camel_case_types)]
pub mod ffi {
    pub use core::ffi::{c_char, c_int, c_longlong, c_schar, c_short, c_uchar, c_uint,
                        c_ulonglong, c_ushort, c_void, CStr};
    pub type c_long = isize;
    pub type c_ulong = usize;
}

/// The kernel crate's, as far as the vendored aref.rs uses it
pub mod prelude {
    pub use core::ffi::c_void;
}

pub mod c {
    #![allow(
        non_camel_case_types,
        non_upper_case_globals,
        non_snake_case,
        dead_code,
        unnecessary_transmutes,
        unsafe_op_in_unsafe_fn, // bindgen's array and union field helpers, at 0.72
        clippy::all // bindgen's output
    )]
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

pub mod print {
    use std::fmt;

    // Userspace's printk drops the loglevel: every one goes to stderr
    pub fn printk(args: fmt::Arguments<'_>) {
        eprint!("{args}");
    }
}

#[macro_export]
macro_rules! pr_info {
    ($($arg:tt)*) => {
        $crate::print::printk(::std::format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! pr_err {
    ($($arg:tt)*) => {
        $crate::print::printk(::std::format_args!($($arg)*))
    };
}
