// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{bits_to_longs, c_default};
use crate::util::ffi::c_char;
use core::ffi::{c_int, c_uint, c_ulong};
use cstruct_macros::{CStruct, c_extern, TypeInfo};

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
    pub fn bch2_write_super_replicas(c: *mut c::bch_fs) -> c_int;
    pub static bch2_sb_features: [*const c_char; 0];
    pub static bch2_sb_compat: [*const c_char; 0];
    pub fn bch2_free_super(sb: *mut c::bch_sb_handle);
    pub fn bch2_sb_realloc(sb: *mut c::bch_sb_handle, u64s: c_uint) -> c_int;
    pub fn bch2_read_super(path: *const c_char, opts: *mut c::bch_opts,
                           sb: *mut c::bch_sb_handle) -> c_int;
    pub fn bch2_read_super_silent(path: *const c_char, opts: *mut c::bch_opts,
                                  sb: *mut c::bch_sb_handle) -> c_int;
    pub fn bch2_sb_update(c: *mut c::bch_fs);
    pub fn bch2_write_super_flags(c: *mut c::bch_fs, flags: c::bch_sb_write_flags) -> c_int;
    pub fn bch2_write_super(c: *mut c::bch_fs) -> c_int;
}
