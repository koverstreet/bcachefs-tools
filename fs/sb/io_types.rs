// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::bits_to_longs;
use crate::published;
use crate::util::projections::Volatile;
use crate::util::ffi::c_char;
use core::ffi::{c_uint, c_ulong};
use cstruct_macros::{CStruct, c_extern, TypeInfo};

/*
 * Updated by sb_update(): what's read of the superblock without sb_lock,
 * a field at a time - written only under it. sb_lock publishes it.
 */
published! {
#[derive(CStruct, TypeInfo)]
pub struct bch_sb_cpu {
    #[c("__uuid_t uuid")]
    pub uuid: [Volatile<u8>; 16],
    #[c("__uuid_t user_uuid")]
    pub user_uuid: [Volatile<u8>; 16],

    pub version: Volatile<u16>,
    pub version_incompat: Volatile<u16>,
    pub version_incompat_allowed: Volatile<u16>,
    pub version_min: Volatile<u16>,
    pub version_upgrade_complete: Volatile<u16>,

    pub nr_devices: Volatile<u8>,
    pub clean: Volatile<u8>,
    /* true if we've ever had more than one device */
    pub multi_device: Volatile<bool>,
    pub dirents_sanitized: Volatile<bool>,

    pub encryption_type: Volatile<u8>,

    pub extent_type_u64s: [Volatile<u8>; 31],
    pub extent_types_known: Volatile<u8>,
    pub extent_bp_shift: Volatile<u8>,

    pub time_base_lo: Volatile<u64>,
    pub time_base_hi: Volatile<u32>,
    pub time_units_per_sec: Volatile<c_uint>,
    pub nsec_per_time_unit: Volatile<c_uint>,
    pub features: Volatile<u64>,
    pub compat: Volatile<u64>,
    pub recovery_passes_required: Volatile<u64>,
    #[c("unsigned long errors_silent[BITS_TO_LONGS(BCH_FSCK_ERR_MAX)]")]
    pub errors_silent: [Volatile<c_ulong>; bits_to_longs(c::BCH_FSCK_ERR_MAX as usize)],
    pub btrees_lost_data: Volatile<u64>,
    pub btrees_clean: Volatile<u64>,
}
}

c_extern! {
    pub static bch2_sb_features: [*const c_char; 0];
    pub static bch2_sb_compat: [*const c_char; 0];
}
