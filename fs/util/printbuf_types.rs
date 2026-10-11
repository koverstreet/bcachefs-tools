// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use crate::util::ffi::c_char;
use core::ffi::{c_int, c_uint, c_void};
use cstruct_macros::{CStruct, bitfield, c_const, c_enum, c_extern};
use nestify::nest;

c_enum! {
    #[closed]
    pub enum printbuf_si: u32 {
        PRINTBUF_UNITS_2,       /* use binary powers of 2^10 */
        PRINTBUF_UNITS_10,      /* use powers of 10^3 (standard SI) */
    }
}

c_const! {
    #[c_int]
    pub const PRINTBUF_INLINE_TABSTOPS: u32 = 8;
}

nest! {
    #[repr(C)]
    #[derive(CStruct)]
    pub struct printbuf {
        pub buf: *mut c_char,
        pub size: c_uint,
        pub pos: c_uint,
        pub last_newline: c_uint,
        pub last_field: c_uint,
        pub indent: c_uint,
        /*
         * If nonzero, allocations will be done with GFP_ATOMIC:
         */
        pub atomic: u8,
        #[c_bitfield]
        #>[bitfield(u8)]
        pub bits: pub struct printbuf_bits {
            #[bits(1)]
            pub allocation_failure: bool,
            #[bits(1)]
            pub heap_allocated: bool,
            #[bits(1)]
            pub overflow: bool,
            /* Ratelimited or already printed */
            #[bits(1)]
            pub suppress: bool,
            #[bits(1)]
            pub si_units: u32,
            #[bits(1)]
            pub human_readable_units: bool,
            #[bits(1)]
            pub has_indent_or_tabstops: bool,
            #[bits(1)]
            pub may_vmalloc: bool,
        },
        pub nr_tabstops: u8,

        /*
         * Do not modify directly: use printbuf_tabstop_add(),
         * printbuf_tabstop_get()
         */
        pub cur_tabstop: u8,
        pub _tabstops: [u8; c::PRINTBUF_INLINE_TABSTOPS as usize],
    }
}
c_default!(printbuf);

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct printbuf_restore {
    pub pos: c_uint,
    pub last_newline: c_uint,
    pub last_field: c_uint,
    pub indent: c_uint,
    pub cur_tabstop: u8,
}

c_extern! {
    pub fn bch2_printbuf_exit(arg1: *mut c::printbuf);
    pub fn bch2_printbuf_tabstops_reset(arg1: *mut c::printbuf);
    pub fn bch2_printbuf_tabstop_push(arg1: *mut c::printbuf, arg2: c_uint) -> c_int;
    pub fn bch2_printbuf_indent_add(arg1: *mut c::printbuf, arg2: c_uint);
    pub fn bch2_printbuf_indent_sub(arg1: *mut c::printbuf, arg2: c_uint);
    pub fn bch2_prt_newline(arg1: *mut c::printbuf);
    pub fn bch2_prt_tab(arg1: *mut c::printbuf);
    pub fn bch2_prt_tab_rjust(arg1: *mut c::printbuf);
    pub fn bch2_printbuf_tabstop_align(arg1: *mut c::printbuf);
    pub fn bch2_prt_bytes_indented(arg1: *mut c::printbuf, arg2: *const c_char, arg3: c_uint);
    pub fn bch2_prt_human_readable_u64(arg1: *mut c::printbuf, arg2: u64);
    pub fn bch2_prt_units_u64(arg1: *mut c::printbuf, arg2: u64);
    pub fn bch2_prt_bitflags(arg1: *mut c::printbuf, arg2: *const *const c_char, arg3: u64);
    pub fn rust_prt_bytes(out: *mut c::printbuf, b: *const c_void, n: c_uint);
}
