// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use crate::util::ffi::c_char;
use core::ffi::c_int;
use cstruct_macros::CStruct;

/*
 * Fsck errors: inconsistency errors we detect at mount time, and should ideally
 * be able to repair:
 */
#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct fsck_err_state {
    pub id: c::bch_sb_error_id,
    pub nr: u64,
    pub ratelimited: bool,
    pub ret: c_int,
    pub fix: c_int,
    pub last_msg: *mut c_char,
}
c_default!(fsck_err_state);
