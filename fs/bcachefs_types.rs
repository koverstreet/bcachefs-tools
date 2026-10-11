// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use crate::util::ffi::c_char;
use core::ffi::{c_int, c_uint};
use cstruct_macros::{CStruct, c_enum, c_extern};
use typeinfo_macros::TypeInfo;

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_log_msg {
    pub c: *mut c::bch_fs,
    pub loglevel: u8,
    pub m: c::printbuf,
}
c_default!(bch_log_msg);

c_enum! {
    #[closed]
    pub enum kern_loglevels: u32 {
        LOGLEVEL_emerg = 0,
        LOGLEVEL_alert = 1,
        LOGLEVEL_crit = 2,
        LOGLEVEL_err = 3,
        LOGLEVEL_warning = 4,
        LOGLEVEL_notice = 5,
        LOGLEVEL_info = 6,
        LOGLEVEL_debug = 7,
    }
}

c_extern! {
    pub fn __bch2_print(c: *mut c::bch_fs, fmt: *const c_char, ...);
    pub fn bch2_ratelimit_state_init(arg1: *mut c::ratelimit_state);
    pub fn bch2_ratelimit_suppress(arg1: *mut c::bch_fs, arg2: *mut c::ratelimit_state, arg3: *const c_char) -> bool;
    pub fn bch2_rust_warn(file: *const c_char, line: c_uint);
    pub fn __bch2_err_throw(arg1: *mut c::bch_fs, arg2: c_int) -> c_int;
}
