// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of journal/init.h - C gets it as prototypes, in
//! journal/init_gen.h, which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::fs::Dev;
use crate::fs::Fs;
use crate::cstructs::c;
use core::ffi::{c_int, c_uint};
use cstruct_macros::c_extern;

c_extern! {
    pub fn bch2_dev_journal_bucket_delete(ca: *mut Dev, b: u64) -> c_int;
    pub fn bch2_set_nr_journal_buckets(c: *mut Fs, ca: *mut Dev, nr: c_uint) -> c_int;
    pub fn bch2_fs_journal_stop(j: *mut c::journal);
    pub fn bch2_fs_journal_exit(j: *mut c::journal);
    pub fn bch2_fs_journal_init_early(j: *mut c::journal);
    pub fn bch2_fs_journal_init(j: *mut c::journal) -> c_int;
    pub fn bch2_fs_journal_init_rw(j: *mut c::journal) -> c_int;
}
