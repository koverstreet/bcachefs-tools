// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]
// The VFS's: kernel only, as C's #ifndef NO_BCACHEFS_FS
#![cfg(not(NO_BCACHEFS_FS))]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::CStruct;

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct nocow_flush {
    pub cl: *mut c::closure,
    pub ca: *mut c::bch_dev,
    pub bio: c::bio,
}
c_default!(nocow_flush);

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct folio_vec {
    pub fv_folio: *mut c::folio,
    pub fv_offset: usize,
    pub fv_len: usize,
}
c_default!(folio_vec);

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct quota_res {
    pub sectors: u64,
}
