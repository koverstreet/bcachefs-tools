// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of alloc/check.h - C gets it as prototypes, in
//! alloc/check_gen.h, which is generated from this file and check.rs: see
//! fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use core::ffi::c_int;
use cstruct_macros::c_extern;

c_extern! {
    pub fn __bch2_check_freespace_key(trans: *mut c::btree_trans, iter: *mut c::btree_iter, gen: *mut u8,
                                      journal_seq_empty: *mut u64, fsck_flags: c::bch_fsck_flags,
                                      last_flushed: *mut c::wb_maybe_flush) -> c_int;
}
