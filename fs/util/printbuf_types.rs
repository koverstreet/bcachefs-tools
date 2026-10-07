// SPDX-License-Identifier: GPL-2.0

//! The data types of util/printbuf_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{CStruct, bitfield, c_const, c_enum, c_extern};

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

#[bitfield(u8)]
pub struct printbuf_allocation_failure_bits {
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
}

#[repr(C)]
#[derive(CStruct)]
pub struct printbuf {
    pub buf: *mut crate::util::ffi::c_char,
    pub size: core::ffi::c_uint,
    pub pos: core::ffi::c_uint,
    pub last_newline: core::ffi::c_uint,
    pub last_field: core::ffi::c_uint,
    pub indent: core::ffi::c_uint,
    /*
     * If nonzero, allocations will be done with GFP_ATOMIC:
     */
    pub atomic: u8,
    #[c_bitfield]
    pub allocation_failure_bits: printbuf_allocation_failure_bits,
    pub nr_tabstops: u8,

    /*
     * Do not modify directly: use printbuf_tabstop_add(),
     * printbuf_tabstop_get()
     */
    pub cur_tabstop: u8,
    pub _tabstops: [u8; c::PRINTBUF_INLINE_TABSTOPS as usize],
}
c_default!(printbuf);
impl printbuf {
    pub fn allocation_failure(&self) -> bool { let b = self.allocation_failure_bits; b.allocation_failure() }
    pub fn set_allocation_failure(&mut self, v: bool) { let mut b = self.allocation_failure_bits; b.set_allocation_failure(v); self.allocation_failure_bits = b; }
    pub fn heap_allocated(&self) -> bool { let b = self.allocation_failure_bits; b.heap_allocated() }
    pub fn set_heap_allocated(&mut self, v: bool) { let mut b = self.allocation_failure_bits; b.set_heap_allocated(v); self.allocation_failure_bits = b; }
    pub fn overflow(&self) -> bool { let b = self.allocation_failure_bits; b.overflow() }
    pub fn set_overflow(&mut self, v: bool) { let mut b = self.allocation_failure_bits; b.set_overflow(v); self.allocation_failure_bits = b; }
    pub fn suppress(&self) -> bool { let b = self.allocation_failure_bits; b.suppress() }
    pub fn set_suppress(&mut self, v: bool) { let mut b = self.allocation_failure_bits; b.set_suppress(v); self.allocation_failure_bits = b; }
    pub fn si_units(&self) -> u32 { let b = self.allocation_failure_bits; b.si_units() }
    pub fn set_si_units(&mut self, v: u32) { let mut b = self.allocation_failure_bits; b.set_si_units(v); self.allocation_failure_bits = b; }
    pub fn human_readable_units(&self) -> bool { let b = self.allocation_failure_bits; b.human_readable_units() }
    pub fn set_human_readable_units(&mut self, v: bool) { let mut b = self.allocation_failure_bits; b.set_human_readable_units(v); self.allocation_failure_bits = b; }
    pub fn has_indent_or_tabstops(&self) -> bool { let b = self.allocation_failure_bits; b.has_indent_or_tabstops() }
    pub fn set_has_indent_or_tabstops(&mut self, v: bool) { let mut b = self.allocation_failure_bits; b.set_has_indent_or_tabstops(v); self.allocation_failure_bits = b; }
    pub fn may_vmalloc(&self) -> bool { let b = self.allocation_failure_bits; b.may_vmalloc() }
    pub fn set_may_vmalloc(&mut self, v: bool) { let mut b = self.allocation_failure_bits; b.set_may_vmalloc(v); self.allocation_failure_bits = b; }
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct printbuf_restore {
    pub pos: core::ffi::c_uint,
    pub last_newline: core::ffi::c_uint,
    pub last_field: core::ffi::c_uint,
    pub indent: core::ffi::c_uint,
    pub cur_tabstop: u8,
}

// What Rust calls of util/printbuf.h: C gets these as prototypes, in util/printbuf_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_printbuf_exit(arg1: *mut c::printbuf);
    pub fn bch2_printbuf_tabstops_reset(arg1: *mut c::printbuf);
    pub fn bch2_printbuf_tabstop_push(arg1: *mut c::printbuf, arg2: core::ffi::c_uint) -> core::ffi::c_int;
    pub fn bch2_printbuf_indent_add(arg1: *mut c::printbuf, arg2: core::ffi::c_uint);
    pub fn bch2_printbuf_indent_sub(arg1: *mut c::printbuf, arg2: core::ffi::c_uint);
    pub fn bch2_prt_newline(arg1: *mut c::printbuf);
    pub fn bch2_prt_tab(arg1: *mut c::printbuf);
    pub fn bch2_prt_tab_rjust(arg1: *mut c::printbuf);
    pub fn bch2_printbuf_tabstop_align(arg1: *mut c::printbuf);
    pub fn bch2_prt_bytes_indented(arg1: *mut c::printbuf, arg2: *const crate::util::ffi::c_char, arg3: core::ffi::c_uint);
    pub fn bch2_prt_human_readable_u64(arg1: *mut c::printbuf, arg2: u64);
    pub fn bch2_prt_units_u64(arg1: *mut c::printbuf, arg2: u64);
    pub fn bch2_prt_bitflags(arg1: *mut c::printbuf, arg2: *const *const crate::util::ffi::c_char, arg3: u64);
    pub fn rust_prt_bytes(out: *mut c::printbuf, b: *const core::ffi::c_void, n: core::ffi::c_uint);
}
