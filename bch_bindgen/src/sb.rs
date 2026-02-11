use crate::c;

// SbField trait + impls — generated from BCH_SB_FIELDS() x-macro
include!(concat!(env!("OUT_DIR"), "/sb_field_types_gen.rs"));

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
