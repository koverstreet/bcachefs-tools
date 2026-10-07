// SPDX-License-Identifier: GPL-2.0

//! The data types of btree/bkey_methods_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::{CStruct, c_extern, c_verbatim};

c_verbatim!(r#"
struct bch_fs;

struct btree;

struct btree_trans;

struct bkey;

enum btree_node_type;
"#);

/*
 * key_validate: checks validity of @k, returns 0 if good or -EINVAL if bad. If
 * invalid, entire key will be deleted.
 *
 * When invalid, error string is returned via @err. @rw indicates whether key is
 * being read or written; more aggressive checks can be enabled when rw == WRITE.
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct bkey_ops {
    pub key_validate: Option<unsafe extern "C" fn(c: *mut c::bch_fs, k: c::bkey_s_c,
                                                  from: *const c::bkey_validate_context) -> core::ffi::c_int>,
    pub val_to_text: Option<unsafe extern "C" fn(*mut c::printbuf, *mut c::bch_fs, c::bkey_s_c)>,
    pub swab: Option<unsafe extern "C" fn(*const c::bch_fs, c::bkey_s)>,
    pub key_merge: Option<unsafe extern "C" fn(*mut c::bch_fs, c::bkey_s, c::bkey_s_c) -> bool>,
    pub trigger: Option<unsafe extern "C" fn(*mut c::btree_trans, c::btree_trigger_op) -> core::ffi::c_int>,
    pub check_repair: Option<unsafe extern "C" fn(*mut c::btree_trans, *mut c::btree_iter,
                                                  c::btree_id, core::ffi::c_uint, c::bkey_s_c) -> core::ffi::c_int>,
    pub compat: Option<unsafe extern "C" fn(id: c::btree_id, version: core::ffi::c_uint,
                                            big_endian: core::ffi::c_uint, write: core::ffi::c_int,
                                            c::bkey_s)>,

    /* Size of value type when first created: */
    pub min_val_size: core::ffi::c_uint,
}

// What Rust calls of btree/bkey_methods.h: C gets these as prototypes, in btree/bkey_methods_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    #[c("const char * const bch2_bkey_types[]")]
    pub static bch2_bkey_types: [*const crate::util::ffi::c_char; 0usize];
    pub fn bch2_bkey_to_text(arg1: *mut c::printbuf, arg2: *const c::bkey);
    pub fn bch2_bkey_val_to_text(arg1: *mut c::printbuf, arg2: *mut c::bch_fs, arg3: c::bkey_s_c);
}
