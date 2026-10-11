// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of data/checksum.h - C gets it as prototypes, in data/checksum_gen.h,
//! which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use core::ffi::{c_int, c_uint, c_void};
use cstruct_macros::c_extern;

c_extern! {
    pub fn bch2_checksum(arg1: *mut c::bch_fs, arg2: c_uint, arg3: c::nonce, arg4: *const c_void, arg5: usize) -> c::bch_csum;
    pub fn bch2_encrypt(arg1: *mut c::bch_fs, arg2: c_uint, arg3: c::nonce, data: *mut c_void, arg4: usize) -> c_int;
    pub fn bch2_chacha20(arg1: *const c::bch_key, arg2: c::nonce, arg3: *mut c_void, arg4: usize);
    pub fn bch2_request_key(arg1: *mut c::bch_sb, arg2: *mut c::bch_key) -> c_int;
    pub fn bch2_revoke_key(arg1: *mut c::bch_sb) -> c_int;
}
