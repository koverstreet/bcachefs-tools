// SPDX-License-Identifier: GPL-2.0

//! The data types of journal/journal_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{CStruct, c_enum, c_extern, c_verbatim};

c_verbatim!(r#"
struct bch_fs;
"#);

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

c_verbatim!(r#"
struct bch_dev;
"#);

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct journal_block {
    pub j: *mut c::journal,
}
c_default!(journal_block);

// What Rust calls of journal/journal.h: C gets these as prototypes, in journal/journal_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_journal_flush(arg1: *mut c::journal) -> core::ffi::c_int;
}
