// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use core::ffi::c_int;
use cstruct_macros::{CStruct, bitfield, c_enum, c_extern, c_xmacro, TypeInfo};
use nestify::nest;

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

nest! {
    #[repr(C)]
    #[derive(Clone, Copy, Default, CStruct, TypeInfo)]
    pub struct bch_dev_io_failures {
        #[c_anon("")] pub __bitfield_align: [u32; 0],
        pub dev: u8,
        #[c_bitfield]
        #>[bitfield(u8, ty = u32)]
        pub bits: pub struct bch_dev_io_failures_bits {
            csum_nr: u7,
            __pad:   u1,
        },
        pub ec_errcode: i16,
        pub errcode: i16,
    }
}

#[repr(C)]
#[derive(Default, CStruct, TypeInfo)]
pub struct bch_io_failures {
    pub nr: u8,
    #[c("struct bch_dev_io_failures data[BCH_REPLICAS_MAX + 1]")]
    pub data: [c::bch_dev_io_failures; c::BCH_REPLICAS_MAX as usize + 1],

    pub ec_msg: c::printbuf,
}

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

c_enum! {
    #[closed]
    pub enum __bch_read_flags: u32 {
        BCH_READ_FLAGS(n) => __BCH_READ_ ## n,
    }
}

c_enum! {
    #[flags]
    pub enum bch_read_flags: u32 {
        BCH_READ_FLAGS(n) => BCH_READ_ ## n = 1 << (c::__BCH_READ_ ## n as u32),
    }
}

c_extern! {
    pub fn bch2_bkey_durability_safe(c: *const c::bch_fs, k: c::bkey_s_c) -> c::bkey_durability;
    pub fn bch2_bkey_drop_stale_ptrs(trans: *mut c::btree_trans, iter: *mut c::btree_iter,
                                     k: c::bkey_s_c) -> c_int;
    pub fn bch2_cut_front_s(c: *const c::bch_fs, where_: c::bpos, k: c::bkey_s) -> c_int;
    pub fn bch2_cut_back_s(where_: c::bpos, k: c::bkey_s) -> c_int;
}
