// SPDX-License-Identifier: GPL-2.0

//! The data types of fs/str_hash_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{CStruct, c_extern};
use typeinfo_macros::TypeInfo;

#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_hash_info {
    pub inum_snapshot: u32,
    pub type_: u8,
    pub is_31bit: bool,
    pub cf_encoding: *mut c::unicode_map,
    /*
     * For crc32 or crc64 string hashes the first key value of
     * the siphash_key (k0) is used as the key.
     */
    pub siphash_key: c::SIPHASH_KEY,
}
c_default!(bch_hash_info);

// What Rust calls of fs/str_hash.h: C gets these as prototypes, in fs/str_hash_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn __bch2_hash_info_init(arg1: *mut c::bch_fs, arg2: *const c::bch_inode_unpacked) -> c::bch_hash_info;
    pub fn bch2_hash_info_init(arg1: *mut c::bch_fs, arg2: *const c::bch_inode_unpacked, arg3: *mut c::bch_hash_info) -> core::ffi::c_int;
}
