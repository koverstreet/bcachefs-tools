// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::util::vstructs::Head;
use crate::types::{DArray, bits_to_longs, c_default};
use crate::util::bitmap::{AtomicBitArray, BitArray};
use crate::util::ffi::c_char;
use core::ffi::c_int;
use cstruct_macros::{CStruct, bitfield, c_extern, c_typedef, TypeInfo};
use nestify::nest;

#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_sb_handle_holder {
    pub c: *mut c::bch_fs,
}
c_default!(bch_sb_handle_holder);

/// A superblock in memory, in a buffer that grows: kvmalloc()'d, a power of
/// two, zeroes past the superblock. Each of a device's superblock, the
/// filesystem's copy, and what format builds.
#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_sb_buf {
    pub sb:          *mut Head<c::bch_sb>,
    pub buffer_size: usize,
}
c_default!(bch_sb_buf);

nest! {
    #[repr(C)]
    #[derive(CStruct, TypeInfo)]
    pub struct bch_sb_handle {
        #[c_anon("struct bch_sb *sb; size_t buffer_size")]
        pub buf: c::bch_sb_buf,
        pub s_bdev_file: *mut c::file,
        pub bdev: *mut c::block_device,
        pub sb_name: *mut c_char,
        pub bio: *mut c::bio,
        pub holder: *mut c::bch_sb_handle_holder,
        pub mode: c::blk_mode_t,
        #[c_bitfield]
        #>[bitfield(u8, ty = u32)]
        pub bits: pub struct bch_sb_handle_bits {
            have_layout: u1,
            have_bio:    u1,
            fs_sb:       u1,
            __pad:       u5,
        },
        pub seq: u64,
    }
}
c_default!(bch_sb_handle);

c_typedef! {
    #[c("DARRAY(struct bch_sb_handle) bch_sb_handles")]
    pub type bch_sb_handles = DArray<c::bch_sb_handle>;
}

/// A set of devices, by index.
pub type DevsMask = BitArray<{ bits_to_longs(c::BCH_SB_MEMBERS_MAX as usize) }>;

/// One changed under readers - c->devs_online, the allocator's rw_devs[] -
/// declared as such where it's a field: still a struct bch_devs_mask to C.
pub type AtomicDevsMask = AtomicBitArray<{ bits_to_longs(c::BCH_SB_MEMBERS_MAX as usize) }>;

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_devs_mask {
    #[c("unsigned long d[BITS_TO_LONGS(BCH_SB_MEMBERS_MAX)]")]
    pub d: DevsMask,
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
    pub fn bch2_dev_read_identity(bdev: *mut c::block_device,
                                  name: *mut c_char, name_size: usize,
                                  model: *mut c_char, model_size: usize,
                                  serial: *mut c_char, serial_size: usize);
}
