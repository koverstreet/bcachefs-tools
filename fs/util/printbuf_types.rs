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
            allocation_failure:     bool,
            heap_allocated:         bool,
            overflow:               bool,
            /* Ratelimited or already printed */
            suppress:               bool,
            #[bits(1)] pub si_units: c::printbuf_si,
            human_readable_units:   bool,
            has_indent_or_tabstops: bool,
            may_vmalloc:            bool,
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
    pub fn bch2_printbuf_exit(buf: *mut c::printbuf);
    pub fn bch2_printbuf_tabstops_reset(buf: *mut c::printbuf);
    pub fn bch2_printbuf_tabstop_push(buf: *mut c::printbuf, spaces: c_uint) -> c_int;
    pub fn bch2_printbuf_indent_add(buf: *mut c::printbuf, spaces: c_uint);
    pub fn bch2_printbuf_indent_sub(buf: *mut c::printbuf, spaces: c_uint);
    pub fn bch2_prt_newline(buf: *mut c::printbuf);
    pub fn bch2_prt_tab(out: *mut c::printbuf);
    pub fn bch2_prt_tab_rjust(buf: *mut c::printbuf);
    pub fn bch2_printbuf_tabstop_align(buf: *mut c::printbuf);
    pub fn bch2_prt_bytes_indented(out: *mut c::printbuf, str: *const c_char, count: c_uint);
    pub fn bch2_prt_human_readable_u64(out: *mut c::printbuf, v: u64);
    pub fn bch2_prt_units_u64(out: *mut c::printbuf, v: u64);
    pub fn bch2_prt_bitflags(out: *mut c::printbuf, list: *const *const c_char, flags: u64);
    pub fn rust_prt_bytes(out: *mut c::printbuf, b: *const c_void, n: c_uint);
}
