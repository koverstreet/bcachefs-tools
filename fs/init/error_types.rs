// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::locking::Lock;
use crate::new_lock;
use crate::types::{DArray, RustOnly, rust_only};
use crate::util::alloc::KVVec;
use crate::util::ffi::c_char;
use core::ffi::c_int;
use cstruct_macros::{CStruct, c_extern, Init, TypeInfo};
use pin_init::pin_data;

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

#[pin_data]
#[repr(C)]
#[derive(CStruct, TypeInfo, Init)]
pub struct bch_fs_errors {
    pub msgs: DArray<*mut c::fsck_err_state>,
    pub msgs_lock: c::mutex,
    pub msgs_alloc_err: bool,

    pub damaged_paths: DArray<c::fsck_damaged_path>,
    pub damaged_paths_alloc_err: bool,

    /* The superblock's error counts, by id: sb/errors.rs */
    #[pin]
    #[init(RustOnly::new(new_lock!(KVVec::new())))]
    pub(crate) counts: rust_only!(Lock<c::mutex, KVVec<c::bch_sb_error_entry_cpu>>),
}

c_extern! {
    pub fn bch2_damage_record(trans: *mut c::btree_trans, pos: c::bpos, err: c::bch_sb_error_id) -> c_int;
    pub fn bch2_fsck_damaged_path_to_text(out: *mut c::printbuf, trans: *mut c::btree_trans,
                                          p: *const c::fsck_damaged_path);
    pub fn bch2_inum_offset_err_msg_trans_norestart(trans: *mut c::btree_trans, out: *mut c::printbuf,
                                                    subvol: u32, pos: c::bpos) -> c_int;
    #[c_printf(2, 3)]
    pub fn bch2_fs_inconsistent(c: *mut c::bch_fs, fmt: *const c_char, ...) -> bool;
    #[c_printf(2, 3)]
    pub fn bch2_trans_inconsistent(trans: *mut c::btree_trans, fmt: *const c_char, ...) -> bool;
    #[c_printf(3, 4)]
    pub fn bch2_fatal_error(c: *mut c::bch_fs, func: *const c_char, fmt: *const c_char, ...);
    pub fn __bch2_count_fsck_err(c: *mut c::bch_fs, id: c::bch_sb_error_id,
                                 msg: *mut c::printbuf) -> bool;
    pub fn __bch2_inconsistent_error(c: *mut c::bch_fs, out: *mut c::printbuf) -> bool;
    #[c_printf(6, 7)]
    #[cold]
    pub fn __bch2_fsck_err(c: *mut c::bch_fs, trans: *mut c::btree_trans, pos: c::bpos,
                           flags: c::bch_fsck_flags, err: c::bch_sb_error_id, fmt: *const c_char,
                           ...) -> c_int;
    #[c_printf(5, 6)]
    pub fn __bch2_bkey_fsck_err(c: *mut c::bch_fs, k: c::bkey_s_c,
                                from: *const c::bkey_validate_context, err: c::bch_sb_error_id,
                                fmt: *const c_char, ...) -> c_int;
}
