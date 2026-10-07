// SPDX-License-Identifier: GPL-2.0

//! The data types of alloc/accounting_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{DArray, c_default};
use cstruct_macros::CStruct;
use typeinfo_macros::TypeInfo;


#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct accounting_mem_entry {
    pub pos: c::bpos,
    pub bversion: c::bversion,
    pub nr_counters: core::ffi::c_uint,
    #[c("u64 __percpu *v[2]")]
    pub v: [*mut u64; 2],
}
c_default!(accounting_mem_entry);

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_accounting_mem {
    #[c("DARRAY(struct accounting_mem_entry) k")]
    pub k: DArray<c::accounting_mem_entry>,
    pub gc_running: bool,
}
c_default!(bch_accounting_mem);
