// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use zerocopy::byteorder::little_endian as le;
use cstruct_macros::CStruct;
use typeinfo_macros::TypeInfo;

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
