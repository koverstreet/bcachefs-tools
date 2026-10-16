// SPDX-License-Identifier: GPL-2.0

use crate::cstructs::c;
use core::ffi::c_int;
use cstruct_macros::c_extern;

c_extern! {
    pub fn __bch2_err_throw(c: *mut c::bch_fs, err: c_int) -> c_int;
}
