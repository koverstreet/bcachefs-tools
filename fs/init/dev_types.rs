// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{DArray, bits_to_longs, c_default};
use crate::util::ffi::c_char;
use core::ffi::{c_int, c_ulong};
use cstruct_macros::{CStruct, bitfield, c_extern, c_typedef};
use nestify::nest;
use typeinfo_macros::TypeInfo;

#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_sb_handle_holder {
    pub c: *mut c::bch_fs,
}
c_default!(bch_sb_handle_holder);

nest! {
    #[repr(C)]
    #[derive(CStruct, TypeInfo)]
    pub struct bch_sb_handle {
        pub sb: *mut c::bch_sb,
        pub s_bdev_file: *mut c::file,
        pub bdev: *mut c::block_device,
        pub sb_name: *mut c_char,
        pub bio: *mut c::bio,
        pub holder: *mut c::bch_sb_handle_holder,
        pub buffer_size: usize,
        pub mode: c::blk_mode_t,
        #[c_bitfield]
        #>[bitfield(u8, ty = u32)]
        pub bits: pub struct bch_sb_handle_bits {
            #[bits(1)]
            pub have_layout: u32,
            #[bits(1)]
            pub have_bio: u32,
            #[bits(1)]
            pub fs_sb: u32,
            #[bits(5)]
            pub __pad: u8,
        },
        pub seq: u64,
    }
}
c_default!(bch_sb_handle);

c_typedef! {
    #[c("DARRAY(struct bch_sb_handle) bch_sb_handles")]
    pub type bch_sb_handles = DArray<c::bch_sb_handle>;
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_devs_mask {
    #[c("unsigned long d[BITS_TO_LONGS(BCH_SB_MEMBERS_MAX)]")]
    pub d: [c_ulong; bits_to_longs(c::BCH_SB_MEMBERS_MAX as usize)],
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_devs_list {
    pub nr: u8,
    pub data: [u8; c::BCH_BKEY_PTRS_MAX as usize],
}

c_extern! {
    pub fn bch2_dev_in_fs(fs: *mut c::bch_sb_handle, sb: *mut c::bch_sb_handle,
                          opts: *mut c::bch_opts) -> c_int;
    pub fn bch2_dev_add(c: *mut c::bch_fs, path: *const c_char, err: *mut c::printbuf) -> c_int;
    pub fn bch2_dev_resize(c: *mut c::bch_fs, ca: *mut c::bch_dev, nbuckets: u64,
                           err: *mut c::printbuf) -> c_int;
}
