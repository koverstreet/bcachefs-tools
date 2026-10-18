// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of alloc/background.h - C gets it as prototypes, in
//! alloc/background_gen.h, which is generated from this file: see
//! fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::c_extern;

c_extern! {
    pub fn bch2_dev_allocator_set_rw(c: *mut c::bch_fs, ca: *mut c::bch_dev, rw: bool);
    pub fn bch2_recalc_capacity(c: *mut c::bch_fs);
}
