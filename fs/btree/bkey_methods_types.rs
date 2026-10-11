// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::util::ffi::c_char;
use core::ffi::{c_int, c_uint};
use cstruct_macros::{CStruct, c_extern};

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
                                                  from: *const c::bkey_validate_context) -> c_int>,
    pub val_to_text: Option<unsafe extern "C" fn(*mut c::printbuf, *mut c::bch_fs, c::bkey_s_c)>,
    pub swab: Option<unsafe extern "C" fn(*const c::bch_fs, c::bkey_s)>,
    pub key_merge: Option<unsafe extern "C" fn(*mut c::bch_fs, c::bkey_s, c::bkey_s_c) -> bool>,
    pub trigger: Option<unsafe extern "C" fn(*mut c::btree_trans, c::btree_trigger_op) -> c_int>,
    pub check_repair: Option<unsafe extern "C" fn(*mut c::btree_trans, *mut c::btree_iter,
                                                  c::btree_id, c_uint, c::bkey_s_c) -> c_int>,
    pub compat: Option<unsafe extern "C" fn(id: c::btree_id, version: c_uint,
                                            big_endian: c_uint, write: c_int,
                                            c::bkey_s)>,

    /* Size of value type when first created: */
    pub min_val_size: c_uint,
}

c_extern! {
    pub static bch2_bkey_types: [*const c_char; 0];
    pub fn bch2_bpos_to_text(arg1: *mut c::printbuf, arg2: c::bpos);
    pub fn bch2_bkey_to_text(arg1: *mut c::printbuf, arg2: *const c::bkey);
    pub fn bch2_bkey_val_to_text(arg1: *mut c::printbuf, arg2: *mut c::bch_fs, arg3: c::bkey_s_c);
}
