// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of init/recovery.h - C gets it as prototypes, in
//! init/recovery_gen.h, which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use core::ffi::c_int;
use cstruct_macros::c_extern;

c_extern! {
    pub fn bch2_btree_lost_data(c: *mut c::bch_fs, msg: *mut c::printbuf, btree: c::btree_id) -> c_int;
    pub fn bch2_set_btree_clean(c: *mut c::bch_fs, btree: c::btree_id);
    pub fn bch2_clear_btree_clean(c: *mut c::bch_fs, btree: c::btree_id);
}
