// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of journal/validate.h - C gets it as prototypes, in journal/validate_gen.h,
//! which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::c_extern;

c_extern! {
    pub fn bch2_journal_entry_to_text(out: *mut c::printbuf, c: *mut c::bch_fs, entry: *mut c::jset_entry);
}
