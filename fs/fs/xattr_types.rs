// SPDX-License-Identifier: GPL-2.0

//! What C calls of fs/xattr.h's Rust: bkey_ops' methods, defined with C's
//! signatures - see rust_c_extern!.

use cstruct_macros::rust_c_extern;
use crate::c;
use crate::xattr::{bch2_xattr_to_text, bch2_xattr_validate};

rust_c_extern! {
    pub fn bch2_xattr_validate(arg1: *mut c::bch_fs, arg2: c::bkey_s_c, arg3: *const c::bkey_validate_context) -> core::ffi::c_int;
    pub fn bch2_xattr_to_text(arg1: *mut c::printbuf, arg2: *mut c::bch_fs, arg3: c::bkey_s_c);
}
