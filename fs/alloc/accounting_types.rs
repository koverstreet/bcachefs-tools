// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{DArray, c_default};
use core::ffi::c_uint;
use cstruct_macros::{CStruct, TypeInfo};

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct accounting_mem_entry {
    pub pos: c::bpos,
    pub bversion: c::bversion,
    pub nr_counters: c_uint,
    #[c("u64 __percpu *v[2]")]
    pub v: [*mut u64; 2],
}
c_default!(accounting_mem_entry);

#[repr(C)]
#[derive(Default, CStruct, TypeInfo)]
pub struct bch_accounting_mem {
    pub k: DArray<c::accounting_mem_entry>,
    pub gc_running: bool,
}
