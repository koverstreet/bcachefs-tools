// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of btree/cache.h - C gets it as prototypes, in btree/cache_gen.h,
//! which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::util::ffi::c_char;
use core::ffi::c_uint;
use cstruct_macros::c_extern;

c_extern! {
    pub fn bch2_btree_id_str(btree: c::btree_id) -> *const c_char;
    pub fn bch2_btree_id_to_text(out: *mut c::printbuf, btree: c::btree_id);
    pub fn bch2_btree_id_level_to_text(out: *mut c::printbuf, btree: c::btree_id, level: c_uint);
    pub fn bch2_btree_node_to_text(out: *mut c::printbuf, c: *mut c::bch_fs, b: *const c::btree);
}
