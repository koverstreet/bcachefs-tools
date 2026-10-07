// SPDX-License-Identifier: GPL-2.0

//! The data types of btree/bbpos_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{CStruct, bitfield, c_extern, c_verbatim};

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct bbpos {
    pub btree: c::btree_id,
    pub pos: c::bpos,
}
c_default!(bbpos);

c_verbatim!(r#"
#define BBPOS_MIN	BBPOS(0, POS_MIN)
#define BBPOS_MAX	BBPOS(BTREE_ID_NR - 1, SPOS_MAX)
"#);

#[bitfield(u16, repr = zerocopy::byteorder::native_endian::U16, from = zerocopy::byteorder::native_endian::U16::new, into = zerocopy::byteorder::native_endian::U16::get)]
pub struct blbpos_btree_bits {
    #[bits(16)]
    pub btree: u32,
}

/*
 * Layout is padding-free (bpos is __packed __aligned(4)), so a blbpos can be
 * used directly as a memcmp/rhashtable key.
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct blbpos {
    #[c_anon("")] pub __bitfield_align: [u32; 0],
    #[c_bitfield]
    pub btree_bits: blbpos_btree_bits,
    pub level: u16,
    pub pos: c::bpos,
}
impl blbpos {
    pub fn btree(&self) -> u32 { let b = self.btree_bits; b.btree() }
    pub fn set_btree(&mut self, v: u32) { let mut b = self.btree_bits; b.set_btree(v); self.btree_bits = b; }
}

// What Rust calls of btree/bbpos.h: C gets these as prototypes, in btree/bbpos_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_bpos_to_text(arg1: *mut c::printbuf, arg2: c::bpos);
}
