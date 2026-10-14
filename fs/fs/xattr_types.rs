// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::CStruct;
#[cfg(not(NO_BCACHEFS_FS))]
use {crate::util::ffi::c_char, cstruct_macros::c_extern};

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct xattr_search_key {
    pub type_: u8,
    pub name: c::qstr,
}
c_default!(xattr_search_key);

// For listxattr, as xattr.h has it: kernel only.
#[cfg(not(NO_BCACHEFS_FS))]
c_extern! {
    pub fn bch2_xattr_list_prefix(ty: c::bch_xattr_type, dentry: *mut c::dentry) -> *const c_char;
}
