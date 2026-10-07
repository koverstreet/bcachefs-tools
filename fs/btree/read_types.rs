// SPDX-License-Identifier: GPL-2.0

//! The data types of btree/read_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{CStruct, bitfield, c_extern, c_typedef, c_verbatim};

c_verbatim!(r#"
struct bch_fs;
struct btree;
"#);

#[bitfield(u8)]
pub struct btree_read_bio_idx_bits {
    #[bits(7)]
    pub idx: u32,
    #[bits(1)]
    pub __pad: u8,
}

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct btree_read_bio {
    pub c: *mut c::bch_fs,
    pub ca: *mut c::bch_dev, /* stashed at submit; see bch_write_bio */
    pub b: *mut c::btree,
    pub start_time: u64,
    #[c_bitfield]
    pub idx_bits: btree_read_bio_idx_bits,
    /* C's CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS: see types.rs */
    #[cfg(all(__KERNEL__, CONFIG_DEBUG_FS))]
    pub list_idx: core::ffi::c_uint,
    pub pick: c::extent_ptr_decoded,
    pub work: c::work_struct,
    pub bio: c::bio,
}
c_default!(btree_read_bio);
impl btree_read_bio {
    pub fn idx(&self) -> u32 { let b = self.idx_bits; b.idx() }
    pub fn set_idx(&mut self, v: u32) { let mut b = self.idx_bits; b.set_idx(v); self.idx_bits = b; }
}

c_typedef! {
    pub type btree_node_scrub_report_fn = Option<unsafe extern "C" fn(priv_: *mut core::ffi::c_void,
                                                                     dev: core::ffi::c_uint, good: bool)>;
}

// What Rust calls of btree/read.h: C gets these as prototypes, in btree/read_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_encrypt(arg1: *mut c::bch_fs, arg2: core::ffi::c_uint, arg3: c::nonce, data: *mut core::ffi::c_void, arg4: usize) -> core::ffi::c_int;
    pub fn bch2_btree_node_read_done(arg1: *mut c::bch_fs, arg2: *mut c::bch_dev, arg3: *mut c::btree, arg4: *mut c::bch_io_failures, arg5: *mut c::printbuf) -> core::ffi::c_int;
}
