// SPDX-License-Identifier: GPL-2.0

//! The data types of data/extents_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{CStruct, bitfield, c_enum, c_extern, c_xmacro};
use typeinfo_macros::TypeInfo;

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_extent_crc_unpacked {
    pub compressed_size: u32,
    pub uncompressed_size: u32,
    pub live_size: u32,

    pub csum_type: u8,
    pub compression_type: u8,

    pub offset: u16,

    pub nonce: u16,

    pub csum: c::bch_csum,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct extent_ptr_decoded {
    pub has_ec: bool,
    pub do_ec_reconstruct: bool,
    pub crc_retry_nr: u8,
    pub crc: c::bch_extent_crc_unpacked,
    pub ptr: c::bch_extent_ptr,
    pub ec: c::bch_extent_stripe_ptr,
}

#[bitfield(u8)]
pub struct bch_dev_io_failures_csum_nr_bits {
    #[bits(7)]
    pub csum_nr: u32,
    #[bits(1)]
    pub __pad: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_dev_io_failures {
    #[c_anon("")] pub __bitfield_align: [u32; 0],
    pub dev: u8,
    #[c_bitfield]
    pub csum_nr_bits: bch_dev_io_failures_csum_nr_bits,
    pub ec_errcode: i16,
    pub errcode: i16,
}
impl bch_dev_io_failures {
    pub fn csum_nr(&self) -> u32 { let b = self.csum_nr_bits; b.csum_nr() }
    pub fn set_csum_nr(&mut self, v: u32) { let mut b = self.csum_nr_bits; b.set_csum_nr(v); self.csum_nr_bits = b; }
}

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_io_failures {
    pub nr: u8,
    #[c("struct bch_dev_io_failures data[BCH_REPLICAS_MAX + 1]")]
    pub data: [c::bch_dev_io_failures; c::BCH_REPLICAS_MAX as usize + 1],

    pub ec_msg: c::printbuf,
}
c_default!(bch_io_failures);

c_xmacro! {
    BCH_READ_FLAGS(x) {
        (retry_if_stale),
        (may_promote),
        (user_mapped),
        (soft_require_read_device),
        (hard_require_read_device),
        (last_fragment),
        (must_bounce),
        (must_clone),
        (in_retry),
        (no_poison_check),
    }
}

macro_rules! ____bch_read_flags_0 {
    ([$($acc:tt)*] $(($n:tt)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[closed]
            pub enum __bch_read_flags: u32 {
                $($acc)*
                $([<__BCH_READ_ $n>],)*
            }
        }
    } };
}
BCH_READ_FLAGS!(____bch_read_flags_0 []);

macro_rules! __bch_read_flags_0 {
    ([$($acc:tt)*] $(($n:tt)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[flags]
            pub enum bch_read_flags: u32 {
                $($acc)*
                $([<BCH_READ_ $n>] = 1 << (c::[<__BCH_READ_ $n>] as u32),)*
            }
        }
    } };
}
BCH_READ_FLAGS!(__bch_read_flags_0 []);

// What Rust calls of data/extents.h: C gets these as prototypes, in data/extents_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_bkey_durability_safe(arg1: *const c::bch_fs, arg2: c::bkey_s_c) -> c::bkey_durability;
    pub fn bch2_bkey_drop_stale_ptrs(arg1: *mut c::btree_trans, arg2: *mut c::btree_iter, arg3: c::bkey_s_c) -> core::ffi::c_int;
}
