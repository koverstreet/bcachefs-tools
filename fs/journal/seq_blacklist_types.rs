// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of journal/seq_blacklist.h - C gets it as prototypes, in journal/seq_blacklist_gen.h,
//! which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::c_extern;

// What Rust calls of journal/seq_blacklist.h: C gets these as prototypes, in journal/seq_blacklist_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_journal_seq_is_blacklisted(arg1: *mut c::bch_fs, arg2: u64, arg3: bool) -> bool;
}
