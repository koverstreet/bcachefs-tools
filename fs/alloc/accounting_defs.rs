// SPDX-License-Identifier: GPL-2.0

//! The data types of alloc/accounting_defs.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::{c_enum, c_extern};

c_enum! {
    #[closed]
    pub enum bch_accounting_mode: u32 {
        BCH_ACCOUNTING_normal,
        BCH_ACCOUNTING_gc,
        BCH_ACCOUNTING_read,
    }
}

// What Rust calls of alloc/accounting.h: C gets these as prototypes, in
// alloc/accounting_defs_gen.h - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_disk_accounting_mod_normal(trans: *mut c::btree_trans, k: *mut c::disk_accounting_pos, d: *mut i64, nr: core::ffi::c_uint) -> core::ffi::c_int;
    pub fn bch2_disk_accounting_mod_gc(trans: *mut c::btree_trans, k: *mut c::disk_accounting_pos, d: *mut i64, nr: core::ffi::c_uint) -> core::ffi::c_int;
    pub fn rust_bch2_accounting_mem_read(c: *mut c::bch_fs, p: c::bpos, v: *mut u64, nr: core::ffi::c_uint);
}
