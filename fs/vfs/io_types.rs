// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]
// The VFS's: kernel only
#![cfg(kernel)]

use crate::cstructs::c;
use cstruct_macros::MaybeZeroable;

#[repr(C)]
#[derive(Clone, Copy, MaybeZeroable)]
pub struct folio_vec {
    pub fv_folio: *mut c::folio,
    pub fv_offset: usize,
    pub fv_len: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable)]
pub struct quota_res {
    pub sectors: u64,
}
