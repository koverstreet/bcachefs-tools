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
    /// Opt value @v's type and level.
    pub fn bits(v: u8) -> bch_compression_opt_bits {
        // SAFETY: every bit pattern is a value of both
        unsafe { bch_compression_opt { value: v }.fields.bits }
    }

    /// The opt value of type @t, at @level.
    pub fn value(t: u8, level: u8) -> u8 {
        let bits = bch_compression_opt_bits::new().with_type_(t).with_level(level);
        // SAFETY: as bits()
        unsafe { bch_compression_opt { fields: bch_compression_opt_fields { bits } }.value }
    }

    /// The compression type opt type @t writes, if it's one: C's
    /// __bch2_compression_opt_to_type[].
    pub fn type_of(t: u8) -> Option<c::bch_compression_type> {
        macro_rules! map {
            ($(($t:ident, $n:literal)),* $(,)?) => { ::paste::paste! {
                match t {
                    $($n => Some(c::bch_compression_type::[<BCH_COMPRESSION_TYPE_ $t>]),)*
                    _ => None,
                }
            } };
        }
        c::BCH_COMPRESSION_OPTS!(map)
    }

    /// A type there is, and a level only with one.
    pub fn valid(v: u8) -> bool {
        let bits = Self::bits(v);
        Self::type_of(bits.type_()).is_some() && !(bits.type_() == 0 && bits.level() != 0)
    }

    /// The compression type @v writes: bch2_compression_opt_to_type(). An
    /// unknown type is none - options are validated, so that's warned about.
    pub fn to_type(v: u8, fs: &Fs) -> c::bch_compression_type {
        Self::type_of(Self::bits(v).type_()).unwrap_or_else(|| {
            crate::warn_once!(true, fs, "unknown compression opt {v}");
            c::bch_compression_type::BCH_COMPRESSION_TYPE_none
        })
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
