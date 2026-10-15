// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::Le16;
use cstruct_macros::{c_enum, whole, CStruct, TypeInfo};

/* An xattr's type - its namespace; Xattr.x_type: */
c_enum! {
    #[open]
    pub enum bch_xattr_type: u32 {
        KEY_TYPE_XATTR_INDEX_USER              = 0,
        KEY_TYPE_XATTR_INDEX_POSIX_ACL_ACCESS  = 1,
        KEY_TYPE_XATTR_INDEX_POSIX_ACL_DEFAULT = 2,
        KEY_TYPE_XATTR_INDEX_TRUSTED           = 3,
        KEY_TYPE_XATTR_INDEX_SECURITY          = 4,
    }
}

#[whole(tail = x_name_and_value: [u8])]
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
#[c_name(bch_xattr)]
pub struct Xattr {
    pub v: c::bch_val,
    pub x_type: u8,
    pub x_name_len: u8,
    pub x_val_len: Le16,
    /*
     * x_name contains the name and value counted by
     * x_name_len + x_val_len. The introduction of
     * __counted_by(x_name_len) previously caused a false positive
     * detection of an out of bounds write.
     */
    pub x_name_and_value: [u8; 0],
}
