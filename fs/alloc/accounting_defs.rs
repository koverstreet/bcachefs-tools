// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use core::ffi::{c_int, c_uint};
use cstruct_macros::{c_enum, c_extern};

c_enum! {
    #[closed]
    pub enum bch_accounting_mode: u32 {
        BCH_ACCOUNTING_normal,
        BCH_ACCOUNTING_gc,
        BCH_ACCOUNTING_read,
    }
}

c_extern! {
    pub fn bch2_disk_accounting_mod_normal(trans: *mut c::btree_trans,
                                           k: *mut c::disk_accounting_pos, d: *mut i64,
                                           nr: c_uint) -> c_int;
    pub fn bch2_disk_accounting_mod_gc(trans: *mut c::btree_trans, k: *mut c::disk_accounting_pos,
                                       d: *mut i64, nr: c_uint) -> c_int;
    pub fn rust_bch2_accounting_mem_read(c: *mut c::bch_fs, p: c::bpos, v: *mut u64, nr: c_uint);
    pub fn bch2_accounting_mem_gc(c: *mut c::bch_fs);
    pub fn bch2_accounting_mem_has_replicas(c: *mut c::bch_fs,
                                            r: *const crate::util::vstructs::Head<c::bch_replicas_entry_v1>)
        -> bool;
}
