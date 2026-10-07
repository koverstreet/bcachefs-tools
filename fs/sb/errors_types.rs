// SPDX-License-Identifier: GPL-2.0

//! The data types of sb/errors_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::DArray;
use cstruct_macros::{CStruct, bitfield, c_extern, c_typedef};
use typeinfo_macros::TypeInfo;


#[bitfield(u64, repr = zerocopy::byteorder::native_endian::U64, from = zerocopy::byteorder::native_endian::U64::new, into = zerocopy::byteorder::native_endian::U64::get)]
pub struct bch_sb_error_entry_cpu_id_bits {
    #[bits(16)]
    pub id: u64,
    #[bits(48)]
    pub nr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_sb_error_entry_cpu {
    #[c_bitfield]
    pub id_bits: bch_sb_error_entry_cpu_id_bits,
    pub first_error_time: u64,
    pub last_error_time: u64,
}
impl bch_sb_error_entry_cpu {
    pub fn id(&self) -> u64 { let b = self.id_bits; b.id() }
    pub fn set_id(&mut self, v: u64) { let mut b = self.id_bits; b.set_id(v); self.id_bits = b; }
    pub fn nr(&self) -> u64 { let b = self.id_bits; b.nr() }
    pub fn set_nr(&mut self, v: u64) { let mut b = self.id_bits; b.set_nr(v); self.id_bits = b; }
}

c_typedef! {
    #[c("DARRAY(struct bch_sb_error_entry_cpu) bch_sb_errors_cpu")]
    pub type bch_sb_errors_cpu = DArray<c::bch_sb_error_entry_cpu>;
}

// What Rust calls of sb/errors.h: C gets these as prototypes, in sb/errors_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    #[c("const char * const bch2_sb_error_strs[]")]
    pub static bch2_sb_error_strs: [*const crate::util::ffi::c_char; 0usize];
    pub fn bch2_sb_error_count(arg1: *mut c::bch_fs, arg2: c::bch_sb_error_id);
}
