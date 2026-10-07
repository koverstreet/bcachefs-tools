// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of data/checksum.h - C gets it as prototypes, in data/checksum_gen.h,
//! which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::c_extern;

// What Rust calls of data/checksum.h: C gets these as prototypes, in data/checksum_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_checksum(arg1: *mut c::bch_fs, arg2: core::ffi::c_uint, arg3: c::nonce, arg4: *const core::ffi::c_void, arg5: usize) -> c::bch_csum;
    pub fn bch2_chacha20(arg1: *const c::bch_key, arg2: c::nonce, arg3: *mut core::ffi::c_void, arg4: usize);
    pub fn bch2_request_key(arg1: *mut c::bch_sb, arg2: *mut c::bch_key) -> core::ffi::c_int;
    pub fn bch2_revoke_key(arg1: *mut c::bch_sb) -> core::ffi::c_int;
}
