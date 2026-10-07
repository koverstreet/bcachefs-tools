// SPDX-License-Identifier: GPL-2.0

//! The data types of init/dev_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{DArray, bits_to_longs, c_default};
use cstruct_macros::{CStruct, bitfield, c_extern, c_typedef, c_verbatim};
use typeinfo_macros::TypeInfo;

c_verbatim!(r#"
#include "util/darray.h"
struct bch_fs;
"#);

#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_sb_handle_holder {
    pub c: *mut c::bch_fs,
}
c_default!(bch_sb_handle_holder);

#[bitfield(u8)]
pub struct bch_sb_handle_have_layout_bits {
    #[bits(1)]
    pub have_layout: u32,
    #[bits(1)]
    pub have_bio: u32,
    #[bits(1)]
    pub fs_sb: u32,
    #[bits(5)]
    pub __pad: u8,
}

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_sb_handle {
    pub sb: *mut c::bch_sb,
    pub s_bdev_file: *mut c::file,
    pub bdev: *mut c::block_device,
    pub sb_name: *mut crate::util::ffi::c_char,
    pub bio: *mut c::bio,
    pub holder: *mut c::bch_sb_handle_holder,
    pub buffer_size: usize,
    pub mode: c::blk_mode_t,
    #[c_bitfield]
    pub have_layout_bits: bch_sb_handle_have_layout_bits,
    pub seq: u64,
}
c_default!(bch_sb_handle);
impl bch_sb_handle {
    pub fn have_layout(&self) -> u32 { let b = self.have_layout_bits; b.have_layout() }
    pub fn set_have_layout(&mut self, v: u32) { let mut b = self.have_layout_bits; b.set_have_layout(v); self.have_layout_bits = b; }
    pub fn have_bio(&self) -> u32 { let b = self.have_layout_bits; b.have_bio() }
    pub fn set_have_bio(&mut self, v: u32) { let mut b = self.have_layout_bits; b.set_have_bio(v); self.have_layout_bits = b; }
    pub fn fs_sb(&self) -> u32 { let b = self.have_layout_bits; b.fs_sb() }
    pub fn set_fs_sb(&mut self, v: u32) { let mut b = self.have_layout_bits; b.set_fs_sb(v); self.have_layout_bits = b; }
}

c_typedef! {
    #[c("DARRAY(struct bch_sb_handle) bch_sb_handles")]
    pub type bch_sb_handles = DArray<c::bch_sb_handle>;
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_devs_mask {
    #[c("unsigned long d[BITS_TO_LONGS(BCH_SB_MEMBERS_MAX)]")]
    pub d: [core::ffi::c_ulong; bits_to_longs(c::BCH_SB_MEMBERS_MAX as usize)],
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_devs_list {
    pub nr: u8,
    pub data: [u8; c::BCH_BKEY_PTRS_MAX as usize],
}

// What Rust calls of init/dev.h: C gets these as prototypes, in init/dev_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_dev_in_fs(arg1: *mut c::bch_sb_handle, arg2: *mut c::bch_sb_handle, arg3: *mut c::bch_opts) -> core::ffi::c_int;
    pub fn bch2_dev_add(arg1: *mut c::bch_fs, arg2: *const crate::util::ffi::c_char, arg3: *mut c::printbuf) -> core::ffi::c_int;
    pub fn bch2_dev_resize(arg1: *mut c::bch_fs, arg2: *mut c::bch_dev, arg3: u64, arg4: *mut c::printbuf) -> core::ffi::c_int;
}
