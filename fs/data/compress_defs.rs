// SPDX-License-Identifier: GPL-2.0

//! The data types of data/compress_defs.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{bitfield, c_enum, CStruct};
use nestify::nest;
use typeinfo_macros::TypeInfo;

nest! {
    #[derive(Clone, Copy, CStruct, TypeInfo)]*
    #[repr(C)]
    pub union bch_compression_opt {
        pub value: u8,
        #[c_anon]
        #>[repr(C)]
        pub fields: pub struct bch_compression_opt_fields {
            #[c_bitfield]
            #>[derive(Clone, Copy, CStruct, TypeInfo)]-
            #>[bitfield(u8)]
            pub type_bits: pub struct bch_compression_opt_type_bits {
                #[bits(4)]
                pub type_: u8,
                #[bits(4)]
                pub level: u8,
            },
        },
    }
}
c_default!(bch_compression_opt);
impl bch_compression_opt_fields {
    pub fn type_(&self) -> u8 { let b = self.type_bits; b.type_() }
    pub fn set_type_(&mut self, v: u8) { let mut b = self.type_bits; b.set_type_(v); self.type_bits = b; }
    pub fn level(&self) -> u8 { let b = self.type_bits; b.level() }
    pub fn set_level(&mut self, v: u8) { let mut b = self.type_bits; b.set_level(v); self.type_bits = b; }
}

c_enum! {
    #[closed]
    pub enum bbuf_type: u32 {
        BB_none,
        BB_vmap,
        BB_kmalloc,
        BB_mempool,
    }
}

/*
 * Bounce buffer for the encode/decode paths: either a direct mapping of a
 * bio's pages (BB_none/BB_vmap - no copy) or a private allocation the caller
 * can scribble on without touching the bio. Exported because the write path
 * assembles its own decode sequence out of these.
 */
#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct bbuf {
    pub c: *mut c::bch_fs,
    pub b: *mut core::ffi::c_void,
    pub type_: c::bbuf_type,
    pub rw: core::ffi::c_int,
}
c_default!(bbuf);
