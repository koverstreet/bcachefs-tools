// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{DArray, c_default};
use crate::util::ffi::c_char;
use core::ffi::c_int;
use cstruct_macros::{CStruct, c_extern};
use typeinfo_macros::TypeInfo;

/*
 * A path that fsck damaged or couldn't fully recover, remembered so we can
 * print a short per-path summary at the end instead of burying it in the error
 * firehose. Keyed by (inum, snapshot); reasons is a bitmask of enum
 * bch_fsck_damage_type - deduped and OR'd on insert, so one line per path lists
 * everything that happened to it, and re-recording on transaction restart is a
 * no-op.
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct fsck_damaged_path {
    pub inum: u64,
    pub snapshot: u32,
    /* bch_sb_error_id, deduped; 0 terminated (error 0 is fs-level) */
    pub nr_errors: u16,
    pub errors: [u16; 7],
}

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_fs_errors {
    pub msgs: DArray<*mut c::fsck_err_state>,
    pub msgs_lock: c::mutex,
    pub msgs_alloc_err: bool,

    pub damaged_paths: DArray<c::fsck_damaged_path>,
    pub damaged_paths_alloc_err: bool,

    pub counts: c::bch_sb_errors_cpu,
    pub counts_lock: c::mutex,
}
c_default!(bch_fs_errors);

c_extern! {
    pub fn bch2_fs_inconsistent(c: *mut c::bch_fs, fmt: *const c_char, ...) -> bool;
    pub fn bch2_trans_inconsistent(trans: *mut c::btree_trans, fmt: *const c_char, ...) -> bool;
    pub fn __bch2_count_fsck_err(c: *mut c::bch_fs, id: c::bch_sb_error_id,
                                 msg: *mut c::printbuf) -> bool;
    pub fn __bch2_fsck_err(c: *mut c::bch_fs, trans: *mut c::btree_trans, pos: c::bpos,
                           flags: c::bch_fsck_flags, err: c::bch_sb_error_id, fmt: *const c_char,
                           ...) -> c_int;
    pub fn __bch2_bkey_fsck_err(c: *mut c::bch_fs, k: c::bkey_s_c,
                                from: *const c::bkey_validate_context, err: c::bch_sb_error_id,
                                fmt: *const c_char, ...) -> c_int;
}
