// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of data/io_misc.h - C gets it as prototypes, in
//! data/io_misc_gen.h, which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use core::ffi::c_int;
use cstruct_macros::c_extern;

c_extern! {
    pub fn bch2_fpunch_snapshot(trans: *mut c::btree_trans, start: c::bpos, end: c::bpos) -> c_int;
    pub fn bch2_fpunch(c: *mut c::bch_fs, inum: c::subvol_inum, start: u64, end: u64,
                       i_sectors_delta: *mut i64) -> c_int;
    pub fn bch2_truncate(c: *mut c::bch_fs, inum: c::subvol_inum, new_i_size: u64,
                         i_sectors_delta: *mut u64) -> c_int;
    pub fn bch2_resume_logged_op_truncate(trans: *mut c::btree_trans, op_k: *mut c::bkey_i) -> c_int;
    pub fn bch2_resume_logged_op_finsert(trans: *mut c::btree_trans, op_k: *mut c::bkey_i) -> c_int;
}
