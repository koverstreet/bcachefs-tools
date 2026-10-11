// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{bits_to_longs, c_default};
use crate::util::ffi::c_char;
use core::ffi::{c_int, c_uint, c_ulong};
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
    pub time_units_per_sec: c_uint,
    pub nsec_per_time_unit: c_uint,
    pub features: u64,
    pub compat: u64,
    pub recovery_passes_required: u64,
    #[c("unsigned long errors_silent[BITS_TO_LONGS(BCH_FSCK_ERR_MAX)]")]
    pub errors_silent: [c_ulong; bits_to_longs(c::BCH_FSCK_ERR_MAX as usize)],
    pub btrees_lost_data: u64,
    pub btrees_clean: u64,
}
c_default!(bch_sb_cpu);

c_extern! {
    pub fn bch2_version_to_text(arg1: *mut c::printbuf, arg2: c::bcachefs_metadata_version);
    pub fn bch2_set_version_incompat(arg1: *mut c::bch_fs, arg2: c::bcachefs_metadata_version) -> c_int;
    pub fn __bch2_check_set_feature(arg1: *mut c::bch_fs, arg2: c_uint);
    pub fn bch2_sb_field_get_id(arg1: *mut c::bch_sb, arg2: c::bch_sb_field_type) -> *mut c::bch_sb_field;
    pub fn bch2_sb_field_resize_id(arg1: *mut c::bch_sb_handle, arg2: c::bch_sb_field_type, arg3: c_uint) -> *mut c::bch_sb_field;
    pub fn bch2_sb_field_get_minsize_id(arg1: *mut c::bch_sb_handle, arg2: c::bch_sb_field_type, arg3: c_uint) -> *mut c::bch_sb_field;
    pub fn bch2_sb_field_delete(arg1: *mut c::bch_sb_handle, arg2: c::bch_sb_field_type);
    pub static bch2_sb_fields: [*const c_char; 0];
    pub fn bch2_free_super(arg1: *mut c::bch_sb_handle);
    pub fn bch2_sb_realloc(arg1: *mut c::bch_sb_handle, arg2: c_uint) -> c_int;
    pub fn bch2_sb_validate(arg1: *mut c::bch_sb, arg2: *mut c::bch_opts, arg3: u64, arg4: c::bch_validate_flags, arg5: *mut c::printbuf) -> c_int;
    pub fn bch2_read_super(arg1: *const c_char, arg2: *mut c::bch_opts, arg3: *mut c::bch_sb_handle) -> c_int;
    pub fn bch2_read_super_silent(arg1: *const c_char, arg2: *mut c::bch_opts, arg3: *mut c::bch_sb_handle) -> c_int;
    pub fn bch2_sb_update(arg1: *mut c::bch_fs);
    pub fn bch2_write_super_flags(arg1: *mut c::bch_fs, arg2: c::bch_sb_write_flags) -> c_int;
    pub fn bch2_write_super(arg1: *mut c::bch_fs) -> c_int;
    pub fn __bch2_sb_field_to_text(arg1: *mut c::printbuf, arg2: *mut c::bch_fs, arg3: *mut c::bch_sb, arg4: *mut c::bch_sb_field);
    pub fn bch2_sb_to_text(arg1: *mut c::printbuf, arg2: *mut c::bch_fs, arg3: *mut c::bch_sb, arg4: bool, arg5: c_uint);
}
