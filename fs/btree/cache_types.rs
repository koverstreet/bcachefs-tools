// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of btree/cache.h - C gets it as prototypes, in btree/cache_gen.h,
//! which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::c_extern;

// What Rust calls of btree/cache.h: C gets these as prototypes, in btree/cache_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_btree_id_str(arg1: c::btree_id) -> *const crate::util::ffi::c_char;
    pub fn bch2_btree_id_to_text(arg1: *mut c::printbuf, arg2: c::btree_id);
    pub fn bch2_btree_id_level_to_text(arg1: *mut c::printbuf, arg2: c::btree_id, arg3: core::ffi::c_uint);
    pub fn bch2_btree_node_to_text(arg1: *mut c::printbuf, arg2: *mut c::bch_fs, arg3: *const c::btree);
}
