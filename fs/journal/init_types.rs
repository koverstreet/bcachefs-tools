// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of journal/init.h - C gets it as prototypes, in
//! journal/init_gen.h, which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::fs::Dev;
use crate::fs::Fs;
use crate::cstructs::c;
use crate::errcode::{ret_to_result_void, BchError};
use core::ffi::{c_int, c_uint};
use cstruct_macros::c_extern;

c_extern! {
    pub fn bch2_set_nr_journal_buckets(c: *mut Fs, ca: *mut Dev, nr: c_uint) -> c_int;
    pub fn bch2_fs_journal_stop(j: *mut c::journal);
    pub fn bch2_fs_journal_exit(j: *mut c::journal);
    pub fn bch2_fs_journal_init_rw(j: *mut c::journal) -> c_int;
    pub fn bch2_journal_set_replay_done(j: *mut c::journal);
    pub fn bch2_fs_journal_start(j: *mut c::journal, info: c::journal_start_info) -> c_int;
}

impl c::journal {
    pub fn start(&self, info: c::journal_start_info) -> Result<(), BchError> {
        ret_to_result_void(unsafe { bch2_fs_journal_start(core::ptr::from_ref(self).cast_mut(), info) })
    }

    pub fn set_replay_done(&self) {
        unsafe { bch2_journal_set_replay_done(core::ptr::from_ref(self).cast_mut()) }
    }
}
