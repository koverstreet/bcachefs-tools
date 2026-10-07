// SPDX-License-Identifier: GPL-2.0

//! The data types of debug/debug_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{CStruct, c_extern, c_verbatim};

c_verbatim!(r#"
struct bio;

struct btree;

struct bch_fs;
"#);

#[cfg(CONFIG_DEBUG_FS)]
#[repr(C)]
#[derive(CStruct)]
pub struct dump_iter {
    pub c: *mut c::bch_fs,
    pub list: *mut c::async_obj_list,
    pub id: c::btree_id,
    pub level: core::ffi::c_uint,
    pub from: c::bpos,
    pub prev_node: c::bpos,
    pub iter: u64,

    pub buf: c::printbuf,

    #[c("char __user *ubuf")]
    pub ubuf: *mut crate::util::ffi::c_char, /* destination user buffer */
    pub size: usize, /* size of requested read */
    pub ret: isize, /* bytes read so far */
}
#[cfg(CONFIG_DEBUG_FS)]
c_default!(dump_iter);

// What Rust calls of debug/debug.h: C gets these as prototypes, in debug/debug_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_btree_node_ondisk_to_text(arg1: *mut c::printbuf, arg2: *mut c::bch_fs, arg3: *const c::btree);
}
