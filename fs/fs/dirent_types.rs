// SPDX-License-Identifier: GPL-2.0

//! What C calls of fs/dirent.h's Rust: bkey_ops' methods, defined with C's
//! signatures - see rust_c_extern!.

use cstruct_macros::rust_c_extern;
use crate::c;
use crate::dirent::format::{bch2_dirent_to_text, bch2_dirent_validate};

rust_c_extern! {
    pub fn bch2_dirent_validate(arg1: *mut c::bch_fs, arg2: c::bkey_s_c, arg3: *const c::bkey_validate_context) -> core::ffi::c_int;
    pub fn bch2_dirent_to_text(arg1: *mut c::printbuf, arg2: *mut c::bch_fs, arg3: c::bkey_s_c);
}
