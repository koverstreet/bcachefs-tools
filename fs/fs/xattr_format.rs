// SPDX-License-Identifier: GPL-2.0

//! The data types of fs/xattr_format.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use zerocopy::byteorder::little_endian as le;
use cstruct_macros::{c_const, CStruct};
use typeinfo_macros::TypeInfo;

c_const! {
    #[c_int]
    pub const KEY_TYPE_XATTR_INDEX_USER: u32 = 0;
}

c_const! {
    #[c_int]
    pub const KEY_TYPE_XATTR_INDEX_POSIX_ACL_ACCESS: u32 = 1;
}

c_const! {
    #[c_int]
    pub const KEY_TYPE_XATTR_INDEX_POSIX_ACL_DEFAULT: u32 = 2;
}

c_const! {
    #[c_int]
    pub const KEY_TYPE_XATTR_INDEX_TRUSTED: u32 = 3;
}

c_const! {
    #[c_int]
    pub const KEY_TYPE_XATTR_INDEX_SECURITY: u32 = 4;
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_xattr {
    pub v: c::bch_val,
    pub x_type: u8,
    pub x_name_len: u8,
    pub x_val_len: le::U16,
    /*
     * x_name contains the name and value counted by
     * x_name_len + x_val_len. The introduction of
     * __counted_by(x_name_len) previously caused a false positive
     * detection of an out of bounds write.
     */
    pub x_name_and_value: [u8; 0],
}
