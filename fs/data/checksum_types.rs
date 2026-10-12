// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of data/checksum.h - C gets it as prototypes, in
//! data/checksum_gen.h, which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::util::vstructs::Ffi;
use core::ffi::{c_int, c_uint, c_void};
use cstruct_macros::c_extern;

c_extern! {
    pub fn bch2_checksum(c: *mut c::bch_fs, type_: c_uint, nonce: c::nonce, data: *const c_void,
                         len: usize) -> c::bch_csum;
    pub fn bch2_encrypt(c: *mut c::bch_fs, type_: c_uint, nonce: c::nonce, data: *mut c_void,
                        len: usize) -> c_int;
    pub fn bch2_chacha20(key: *const c::bch_key, nonce: c::nonce, data: *mut c_void, len: usize);
    pub fn bch2_revoke_key(sb: *mut Ffi<c::bch_sb>) -> c_int;
}
