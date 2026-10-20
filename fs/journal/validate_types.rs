// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of journal/validate.h - C gets it as prototypes, in
//! journal/validate_gen.h, which is generated from this file: see
//! fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::util::vstructs::Head;
use cstruct_macros::c_extern;

c_extern! {
    pub fn bch2_journal_entry_validate(c: *mut c::bch_fs, jset: *mut Head<c::jset>,
                                       entry: *mut Head<c::jset_entry>, version: core::ffi::c_uint,
                                       big_endian: core::ffi::c_int,
                                       from: c::bkey_validate_context) -> core::ffi::c_int;
    pub fn bch2_journal_entry_to_text(out: *mut c::printbuf, c: *mut c::bch_fs,
                                      entry: *mut Head<c::jset_entry>);
}
