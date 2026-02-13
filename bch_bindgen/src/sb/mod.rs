pub mod io;

use crate::c;
use crate::bitmask_accessors;

// SbField trait + impls — generated from BCH_SB_FIELDS() x-macro
include!(concat!(env!("OUT_DIR"), "/sb_field_types_gen.rs"));

// Member state name table — generated from BCH_MEMBER_STATES() x-macro
include!(concat!(env!("OUT_DIR"), "/member_states_gen.rs"));

pub fn member_state_str(state: u8) -> &'static str {
    MEMBER_STATE_NAMES.get(state as usize).copied().unwrap_or("unknown")
}

// Counter info table — generated from BCH_PERSISTENT_COUNTERS() x-macro
include!(concat!(env!("OUT_DIR"), "/counters_gen.rs"));

/// Get a typed reference to a superblock field, or None if absent.
pub fn sb_field_get<F: SbField>(sb: &c::bch_sb) -> Option<&F> {
    unsafe {
        let ptr = c::bch2_sb_field_get_id(sb as *const _ as *mut _, F::FIELD_TYPE);
        if ptr.is_null() { None } else { Some(&*(ptr as *const F)) }
    }
}

/// Get a typed mutable reference to a superblock field, or None if absent.
///
/// # Safety
/// Caller must ensure exclusive access to the superblock.
pub unsafe fn sb_field_get_mut<'a, F: SbField>(sb: *mut c::bch_sb) -> Option<&'a mut F> {
    let ptr = c::bch2_sb_field_get_id(sb, F::FIELD_TYPE);
    if ptr.is_null() { None } else { Some(&mut *(ptr as *mut F)) }
}

// LE64_BITMASK accessors — pure Rust replacements for C shims in rust_shims.c.
// Each field is defined by: struct type, flags field + index, C constant prefix.

bitmask_accessors! {
    bch_sb, flags[1],
        BCH_SB_ENCRYPTION_TYPE    => (sb_encryption_type, set_sb_encryption_type),
        BCH_SB_META_REPLICAS_REQ  => (sb_meta_replicas_req, set_sb_meta_replicas_req),
        BCH_SB_DATA_REPLICAS_REQ  => (sb_data_replicas_req, set_sb_data_replicas_req),
        BCH_SB_PROMOTE_TARGET     => (sb_promote_target, set_sb_promote_target),
        BCH_SB_FOREGROUND_TARGET  => (sb_foreground_target, set_sb_foreground_target),
        BCH_SB_BACKGROUND_TARGET  => (sb_background_target, set_sb_background_target);

    bch_sb, flags[3],
        BCH_SB_METADATA_TARGET    => (sb_metadata_target, set_sb_metadata_target);

    bch_sb, flags[5],
        BCH_SB_VERSION_INCOMPAT_ALLOWED => (sb_version_incompat_allowed, set_sb_version_incompat_allowed);

    bch_sb, flags[6],
        BCH_SB_EXTENT_BP_SHIFT    => (sb_extent_bp_shift, set_sb_extent_bp_shift);

    bch_member, flags,
        BCH_MEMBER_STATE          => (member_state, set_member_state),
        BCH_MEMBER_GROUP          => (member_group, set_member_group),
        BCH_MEMBER_ROTATIONAL_SET => (member_rotational_set, set_member_rotational_set);
}
