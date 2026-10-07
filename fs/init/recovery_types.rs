// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of init/recovery.h - C gets it as prototypes, in init/recovery_gen.h,
//! which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::c_extern;

// What Rust calls of init/recovery.h: C gets these as prototypes, in init/recovery_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_set_btree_clean(arg1: *mut c::bch_fs, arg2: c::btree_id);
    pub fn bch2_clear_btree_clean(arg1: *mut c::bch_fs, arg2: c::btree_id);
}
