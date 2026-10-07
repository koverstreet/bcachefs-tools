// SPDX-License-Identifier: GPL-2.0

//! The data types of vfs/io_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::CStruct;

#[cfg(not(NO_BCACHEFS_FS))]
#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct nocow_flush {
    pub cl: *mut c::closure,
    pub ca: *mut c::bch_dev,
    pub bio: c::bio,
}
#[cfg(not(NO_BCACHEFS_FS))]
c_default!(nocow_flush);

#[cfg(not(NO_BCACHEFS_FS))]
#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct folio_vec {
    pub fv_folio: *mut c::folio,
    pub fv_offset: usize,
    pub fv_len: usize,
}
#[cfg(not(NO_BCACHEFS_FS))]
c_default!(folio_vec);

#[cfg(not(NO_BCACHEFS_FS))]
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct quota_res {
    pub sectors: u64,
}
