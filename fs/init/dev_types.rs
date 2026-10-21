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
    /// The section types edited since the last write, by bit: set by
    /// field_mut() and resizing, for the filesystem's to_cpu() to read.
    pub dirty:       u64,
}
c_default!(bch_sb_buf);

/// The filesystem's superblock, c->disk_sb: what its devices' are copied
/// from. Edited in place through its bch_sb_buf; resized only through
/// Fs::sb_resize(), which grows its devices' with it.
#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_fs_sb {
    #[c_anon("struct bch_sb *sb; size_t buffer_size; u64 dirty")]
    buf: c::bch_sb_buf,
}
c_default!(bch_fs_sb);

impl core::ops::Deref for bch_fs_sb {
    type Target = c::bch_sb_buf;

    fn deref(&self) -> &c::bch_sb_buf {
        &self.buf
    }
}

impl core::ops::DerefMut for bch_fs_sb {
    fn deref_mut(&mut self) -> &mut c::bch_sb_buf {
        &mut self.buf
    }
}

nest! {
    /// A device's superblock, and what reads and writes it: what changes
    /// while the device is a filesystem's, under its sb_lock.
    #[repr(C)]
    #[derive(CStruct, TypeInfo)]
    pub struct bch_sb_io {
        #[c_anon("struct bch_sb *sb; size_t buffer_size; u64 dirty")]
        pub buf: c::bch_sb_buf,
        pub bio: *mut c::bio,
        #[c_bitfield]
        #>[bitfield(u8, ty = u32)]
        pub bits: pub struct bch_sb_handle_bits {
            have_layout: u1,
            have_bio:    u1,
            __pad:       u6,
        },
        /// The last seq written
        pub seq: u64,
    }
}
c_default!(bch_sb_io);

/// The block device a superblock was read from, open: fixed while the
/// device is a filesystem's.
#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_sb_bdev {
    pub s_bdev_file: *mut c::file,
    pub bdev: *mut c::block_device,
    pub sb_name: *mut c_char,
    pub holder: *mut c::bch_sb_handle_holder,
    pub mode: c::blk_mode_t,
}
c_default!(bch_sb_bdev);

/// A superblock read from a device: bch2_read_super()'s. To C, one struct
/// of both halves' members.
#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_sb_handle {
    #[c_anon]
    pub io: bch_sb_io,
    #[c_anon]
    pub dev: bch_sb_bdev,
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
    pub fn bch2_dev_in_fs(fs: *mut Head<c::bch_sb>, fs_bdev: *mut c::block_device,
                          sb: *mut c::bch_sb_handle, opts: *mut c::bch_opts) -> c_int;
    pub fn bch2_dev_add(c: *mut c::bch_fs, path: *const c_char, err: *mut c::printbuf) -> c_int;
    pub fn bch2_dev_add_initialize(c: *mut c::bch_fs, ca: *mut c::bch_dev) -> c_int;
    pub fn bch2_dev_resize(c: *mut c::bch_fs, ca: *mut c::bch_dev, nbuckets: u64,
                           err: *mut c::printbuf) -> c_int;
    pub static bch2_sb_handle_bdev_ops: c::blk_holder_ops;
    pub fn bch2_dev_read_identity(bdev: *mut c::block_device,
                                  name: *mut c_char, name_size: usize,
                                  model: *mut c_char, model_size: usize,
                                  serial: *mut c_char, serial_size: usize);
}
