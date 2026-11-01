// SPDX-License-Identifier: GPL-2.0

use crate::fs::Fs;
use core::ffi::c_int;
use cstruct_macros::c_extern;

c_extern! {
    pub fn __bch2_err_throw(c: *mut Fs, err: c_int) -> c_int;
}
