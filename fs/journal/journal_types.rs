// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use core::ffi::c_int;
use cstruct_macros::{CStruct, c_enum, c_extern};

c_enum! {
    #[flags]
    pub enum journal_cycle_flags: u32 {
        JOURNAL_CYCLE_must_close = 1 << 0,
        JOURNAL_CYCLE_must_open = 1 << 1,
        JOURNAL_CYCLE_force_close = 1 << 2,
    }
}

/* First bits for BCH_WATERMARK: */
c_enum! {
    #[closed]
    pub enum journal_res_flags: u32 {
        __JOURNAL_RES_GET_NONBLOCK = c::BCH_WATERMARK_BITS,
        __JOURNAL_RES_GET_CHECK,
    }
}

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct journal_block {
    pub j: *mut c::journal,
}
c_default!(journal_block);

c_extern! {
    pub fn bch2_journal_flush(j: *mut c::journal) -> c_int;
}
