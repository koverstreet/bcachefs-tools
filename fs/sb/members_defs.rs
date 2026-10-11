// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::types::c_default;
use crate::util::ffi::c_char;
use cstruct_macros::CStruct;
use typeinfo_macros::TypeInfo;

#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_dev_identity {
    #[c("char name[sizeof(((struct bch_member *) NULL) -> device_name) + 1]")]
    pub name: [c_char; 16 + 1],
    #[c("char model[sizeof(((struct bch_member *) NULL) -> device_model) + 1]")]
    pub model: [c_char; 64 + 1],
    #[c("char serial[sizeof(((struct bch_member *) NULL) -> device_serial) + 1]")]
    pub serial: [c_char; 64 + 1],
    pub rotational: bool,
}
c_default!(bch_dev_identity);
