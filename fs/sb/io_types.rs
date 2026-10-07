// SPDX-License-Identifier: GPL-2.0

//! The data types of sb/io_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{bits_to_longs, c_default};
use cstruct_macros::{CStruct, c_extern};
use typeinfo_macros::TypeInfo;

/* Updated by bch2_sb_update():*/
#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_sb_cpu {
    pub uuid: c::__uuid_t,
    pub user_uuid: c::__uuid_t,

    pub version: u16,
    pub version_incompat: u16,
    pub version_incompat_allowed: u16,
    pub version_min: u16,
    pub version_upgrade_complete: u16,

    pub nr_devices: u8,
    pub clean: u8,
    pub multi_device: bool, /* true if we've ever had more than one device */
    pub dirents_sanitized: bool,

    pub encryption_type: u8,

    pub extent_type_u64s: [u8; 31],
    pub extent_types_known: u8,
    pub extent_bp_shift: u8,

    pub time_base_lo: u64,
    pub time_base_hi: u32,
    pub time_units_per_sec: core::ffi::c_uint,
    pub nsec_per_time_unit: core::ffi::c_uint,
    pub features: u64,
    pub compat: u64,
    pub recovery_passes_required: u64,
    #[c("unsigned long errors_silent[BITS_TO_LONGS(BCH_FSCK_ERR_MAX)]")]
    pub errors_silent: [core::ffi::c_ulong; bits_to_longs(c::BCH_FSCK_ERR_MAX as usize)],
    pub btrees_lost_data: u64,
    pub btrees_clean: u64,
}
c_default!(bch_sb_cpu);

// What Rust calls of sb/io.h: C gets these as prototypes, in sb/io_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_version_to_text(arg1: *mut c::printbuf, arg2: c::bcachefs_metadata_version);
    pub fn bch2_set_version_incompat(arg1: *mut c::bch_fs, arg2: c::bcachefs_metadata_version) -> core::ffi::c_int;
    pub fn __bch2_check_set_feature(arg1: *mut c::bch_fs, arg2: core::ffi::c_uint);
    pub fn bch2_sb_field_get_id(arg1: *mut c::bch_sb, arg2: c::bch_sb_field_type) -> *mut c::bch_sb_field;
    pub fn bch2_sb_field_resize_id(arg1: *mut c::bch_sb_handle, arg2: c::bch_sb_field_type, arg3: core::ffi::c_uint) -> *mut c::bch_sb_field;
    pub fn bch2_sb_field_get_minsize_id(arg1: *mut c::bch_sb_handle, arg2: c::bch_sb_field_type, arg3: core::ffi::c_uint) -> *mut c::bch_sb_field;
    pub fn bch2_sb_field_delete(arg1: *mut c::bch_sb_handle, arg2: c::bch_sb_field_type);
    #[c("const char * const bch2_sb_fields[]")]
    pub static bch2_sb_fields: [*const crate::util::ffi::c_char; 0usize];
    pub fn bch2_free_super(arg1: *mut c::bch_sb_handle);
    pub fn bch2_sb_realloc(arg1: *mut c::bch_sb_handle, arg2: core::ffi::c_uint) -> core::ffi::c_int;
    pub fn bch2_sb_validate(arg1: *mut c::bch_sb, arg2: *mut c::bch_opts, arg3: u64, arg4: c::bch_validate_flags, arg5: *mut c::printbuf) -> core::ffi::c_int;
    pub fn bch2_read_super(arg1: *const crate::util::ffi::c_char, arg2: *mut c::bch_opts, arg3: *mut c::bch_sb_handle) -> core::ffi::c_int;
    pub fn bch2_read_super_silent(arg1: *const crate::util::ffi::c_char, arg2: *mut c::bch_opts, arg3: *mut c::bch_sb_handle) -> core::ffi::c_int;
    pub fn bch2_sb_update(arg1: *mut c::bch_fs);
    pub fn bch2_write_super_flags(arg1: *mut c::bch_fs, arg2: c::bch_sb_write_flags) -> core::ffi::c_int;
    pub fn bch2_write_super(arg1: *mut c::bch_fs) -> core::ffi::c_int;
    pub fn __bch2_sb_field_to_text(arg1: *mut c::printbuf, arg2: *mut c::bch_fs, arg3: *mut c::bch_sb, arg4: *mut c::bch_sb_field);
    pub fn bch2_sb_to_text(arg1: *mut c::printbuf, arg2: *mut c::bch_fs, arg3: *mut c::bch_sb, arg4: bool, arg5: core::ffi::c_uint);
}
