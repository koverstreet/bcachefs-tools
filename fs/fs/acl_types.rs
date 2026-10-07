// SPDX-License-Identifier: GPL-2.0

//! The data types of fs/acl_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use zerocopy::byteorder::little_endian as le;
use cstruct_macros::{c_verbatim, CStruct};
use typeinfo_macros::TypeInfo;

c_verbatim!(r#"
struct bch_inode_unpacked;

struct bch_hash_info;

struct bch_inode_info;

struct posix_acl;
"#);

#[repr(C, align(4))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_typedef]
pub struct bch_acl_entry {
    pub e_tag: le::U16,
    pub e_perm: le::U16,
    pub e_id: le::U32,
}

#[repr(C, align(2))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_typedef]
pub struct bch_acl_entry_short {
    pub e_tag: le::U16,
    pub e_perm: le::U16,
}

#[repr(C, align(4))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_typedef]
pub struct bch_acl_header {
    pub a_version: le::U32,
}
