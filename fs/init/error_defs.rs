// SPDX-License-Identifier: GPL-2.0

//! The data types of init/error_defs.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{c_verbatim, CStruct};

c_verbatim!(r#"
struct bch_dev;

struct bch_fs;

struct work_struct;
"#);

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
    pub ret: core::ffi::c_int,
    pub fix: core::ffi::c_int,
    pub last_msg: *mut crate::util::ffi::c_char,
}
c_default!(fsck_err_state);

c_verbatim!(r#"
enum bch_validate_flags;
"#);
