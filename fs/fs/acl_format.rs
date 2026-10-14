// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::types::{Le16, Le32};
use cstruct_macros::{CStruct, TypeInfo};

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_typedef]
pub struct bch_acl_entry {
    pub e_tag: Le16,
    pub e_perm: Le16,
    pub e_id: Le32,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_typedef]
pub struct bch_acl_entry_short {
    pub e_tag: Le16,
    pub e_perm: Le16,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_typedef]
pub struct bch_acl_header {
    pub a_version: Le32,
}
