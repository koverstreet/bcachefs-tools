// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of journal/validate.h - C gets it as prototypes, in journal/validate_gen.h,
//! which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::c_extern;

// What Rust calls of journal/validate.h: C gets these as prototypes, in journal/validate_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_journal_entry_to_text(arg1: *mut c::printbuf, arg2: *mut c::bch_fs, arg3: *mut c::jset_entry);
}
