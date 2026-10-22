// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::bits_to_longs;
use crate::published;
use crate::util::projections::Volatile;
use crate::util::ffi::c_char;
use core::ffi::{c_int, c_uint, c_ulong};
use cstruct_macros::{CStruct, c_extern, TypeInfo};

/*
 * Updated by bch2_sb_update(): what's read of the superblock without sb_lock,
 * a field at a time - written only under it. sb_lock publishes it.
 */
published! {
#[derive(CStruct, TypeInfo)]
pub struct bch_sb_cpu {
    #[c("__uuid_t uuid")]
    pub uuid: [Volatile<u8>; 16],
    #[c("__uuid_t user_uuid")]
    pub user_uuid: [Volatile<u8>; 16],

    #[c("u16 version")]
    pub version: Volatile<u16>,
    #[c("u16 version_incompat")]
    pub version_incompat: Volatile<u16>,
    #[c("u16 version_incompat_allowed")]
    pub version_incompat_allowed: Volatile<u16>,
    #[c("u16 version_min")]
    pub version_min: Volatile<u16>,
    #[c("u16 version_upgrade_complete")]
    pub version_upgrade_complete: Volatile<u16>,

    #[c("u8 nr_devices")]
    pub nr_devices: Volatile<u8>,
    #[c("u8 clean")]
    pub clean: Volatile<u8>,
    /* true if we've ever had more than one device */
    #[c("bool multi_device")]
    pub multi_device: Volatile<bool>,
    #[c("bool dirents_sanitized")]
    pub dirents_sanitized: Volatile<bool>,

    #[c("u8 encryption_type")]
    pub encryption_type: Volatile<u8>,

    #[c("u8 extent_type_u64s[31]")]
    pub extent_type_u64s: [Volatile<u8>; 31],
    #[c("u8 extent_types_known")]
    pub extent_types_known: Volatile<u8>,
    #[c("u8 extent_bp_shift")]
    pub extent_bp_shift: Volatile<u8>,

    #[c("u64 time_base_lo")]
    pub time_base_lo: Volatile<u64>,
    #[c("u32 time_base_hi")]
    pub time_base_hi: Volatile<u32>,
    #[c("unsigned time_units_per_sec")]
    pub time_units_per_sec: Volatile<c_uint>,
    #[c("unsigned nsec_per_time_unit")]
    pub nsec_per_time_unit: Volatile<c_uint>,
    #[c("u64 features")]
    pub features: Volatile<u64>,
    #[c("u64 compat")]
    pub compat: Volatile<u64>,
    #[c("u64 recovery_passes_required")]
    pub recovery_passes_required: Volatile<u64>,
    #[c("unsigned long errors_silent[BITS_TO_LONGS(BCH_FSCK_ERR_MAX)]")]
    pub errors_silent: [Volatile<c_ulong>; bits_to_longs(c::BCH_FSCK_ERR_MAX as usize)],
    #[c("u64 btrees_lost_data")]
    pub btrees_lost_data: Volatile<u64>,
    #[c("u64 btrees_clean")]
    pub btrees_clean: Volatile<u64>,
}
}

c_extern! {
    pub fn bch2_write_super_replicas(c: *mut c::bch_fs) -> c_int;
    pub static bch2_sb_features: [*const c_char; 0];
    pub static bch2_sb_compat: [*const c_char; 0];
    pub fn bch2_write_super_flags(c: *mut c::bch_fs, flags: c::bch_sb_write_flags) -> c_int;
    pub fn bch2_write_super(c: *mut c::bch_fs) -> c_int;
}
