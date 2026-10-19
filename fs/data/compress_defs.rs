// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::fs::Fs;
use crate::types::c_default;
use core::ffi::{c_int, c_void};
use cstruct_macros::{bitfield, c_enum, CStruct, TypeInfo};
use nestify::nest;

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
            pub bits: pub struct bch_compression_opt_bits {
                type_: u4,
                level: u4,
            },
        },
    }
}
c_default!(bch_compression_opt);

impl bch_compression_opt {
    /// A type there is, and a level only with one: as
    /// bch2_compression_opt_valid(), compress.h's - a copy, until compress.c
    /// is Rust.
    pub fn valid(v: u8) -> bool {
        // SAFETY: every bit pattern is a value of both
        let bits = unsafe { bch_compression_opt { value: v }.fields.bits };

        (bits.type_() as u32) < c::bch_compression_opts::BCH_COMPRESSION_OPT_NR.0 &&
            !(bits.type_() == 0 && bits.level() != 0)
    }

    /// The compression type @v writes: bch2_compression_opt_to_type(). An
    /// unknown type is none - options are validated, so that's warned about.
    pub fn to_type(v: u8, fs: &Fs) -> c::bch_compression_type {
        use c::bch_compression_opts as o;
        use c::bch_compression_type as t;
        // SAFETY: every bit pattern is a value of both
        let bits = unsafe { bch_compression_opt { value: v }.fields.bits };

        match o(bits.type_().into()) {
            o::BCH_COMPRESSION_OPT_none => t::BCH_COMPRESSION_TYPE_none,
            o::BCH_COMPRESSION_OPT_lz4  => t::BCH_COMPRESSION_TYPE_lz4,
            o::BCH_COMPRESSION_OPT_gzip => t::BCH_COMPRESSION_TYPE_gzip,
            o::BCH_COMPRESSION_OPT_zstd => t::BCH_COMPRESSION_TYPE_zstd,
            _ => {
                crate::warn_once!(true, fs, "unknown compression opt {v}");
                t::BCH_COMPRESSION_TYPE_none
            }
        }
    }
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
    pub b: *mut c_void,
    pub type_: c::bbuf_type,
    pub rw: c_int,
}
c_default!(bbuf);
