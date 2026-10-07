// SPDX-License-Identifier: GPL-2.0

//! The data types of sb/members_defs.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::types::c_default;
use cstruct_macros::{c_verbatim, CStruct};
use typeinfo_macros::TypeInfo;

c_verbatim!(r#"
struct sb_write;
"#);

#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_dev_identity {
    #[c("char name[sizeof(((struct bch_member *) NULL) -> device_name) + 1]")]
    pub name: [crate::util::ffi::c_char; { (core::mem::size_of::<[u8; 16]>() as usize) + 1 }],
    #[c("char model[sizeof(((struct bch_member *) NULL) -> device_model) + 1]")]
    pub model: [crate::util::ffi::c_char; { (core::mem::size_of::<[u8; 64]>() as usize) + 1 }],
    #[c("char serial[sizeof(((struct bch_member *) NULL) -> device_serial) + 1]")]
    pub serial: [crate::util::ffi::c_char; { (core::mem::size_of::<[u8; 64]>() as usize) + 1 }],
    pub rotational: bool,
}
c_default!(bch_dev_identity);
