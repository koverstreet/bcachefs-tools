// Auto-generated from BCH_SB_FIELDS() — do not edit

/// Marker trait connecting an sb field struct to its field type enum.
///
/// # Safety
/// Implementors must ensure FIELD_TYPE matches the struct type,
/// and that `field` is the first member (offset 0).
pub unsafe trait SbField: Sized {
    const FIELD_TYPE: c::bch_sb_field_type;
}

unsafe impl SbField for c::bch_sb_field_journal {
    const FIELD_TYPE: c::bch_sb_field_type = c::bch_sb_field_type::journal;
}

unsafe impl SbField for c::bch_sb_field_members_v1 {
    const FIELD_TYPE: c::bch_sb_field_type = c::bch_sb_field_type::members_v1;
}

unsafe impl SbField for c::bch_sb_field_crypt {
    const FIELD_TYPE: c::bch_sb_field_type = c::bch_sb_field_type::crypt;
}

unsafe impl SbField for c::bch_sb_field_replicas_v0 {
    const FIELD_TYPE: c::bch_sb_field_type = c::bch_sb_field_type::replicas_v0;
}

unsafe impl SbField for c::bch_sb_field_quota {
    const FIELD_TYPE: c::bch_sb_field_type = c::bch_sb_field_type::quota;
}

unsafe impl SbField for c::bch_sb_field_disk_groups {
    const FIELD_TYPE: c::bch_sb_field_type = c::bch_sb_field_type::disk_groups;
}

unsafe impl SbField for c::bch_sb_field_clean {
    const FIELD_TYPE: c::bch_sb_field_type = c::bch_sb_field_type::clean;
}

unsafe impl SbField for c::bch_sb_field_replicas {
    const FIELD_TYPE: c::bch_sb_field_type = c::bch_sb_field_type::replicas;
}

unsafe impl SbField for c::bch_sb_field_journal_seq_blacklist {
    const FIELD_TYPE: c::bch_sb_field_type = c::bch_sb_field_type::journal_seq_blacklist;
}

unsafe impl SbField for c::bch_sb_field_journal_v2 {
    const FIELD_TYPE: c::bch_sb_field_type = c::bch_sb_field_type::journal_v2;
}

unsafe impl SbField for c::bch_sb_field_counters {
    const FIELD_TYPE: c::bch_sb_field_type = c::bch_sb_field_type::counters;
}

unsafe impl SbField for c::bch_sb_field_members_v2 {
    const FIELD_TYPE: c::bch_sb_field_type = c::bch_sb_field_type::members_v2;
}

unsafe impl SbField for c::bch_sb_field_errors {
    const FIELD_TYPE: c::bch_sb_field_type = c::bch_sb_field_type::errors;
}

unsafe impl SbField for c::bch_sb_field_ext {
    const FIELD_TYPE: c::bch_sb_field_type = c::bch_sb_field_type::ext;
}

unsafe impl SbField for c::bch_sb_field_downgrade {
    const FIELD_TYPE: c::bch_sb_field_type = c::bch_sb_field_type::downgrade;
}

unsafe impl SbField for c::bch_sb_field_recovery_passes {
    const FIELD_TYPE: c::bch_sb_field_type = c::bch_sb_field_type::recovery_passes;
}

unsafe impl SbField for c::bch_sb_field_extent_type_u64s {
    const FIELD_TYPE: c::bch_sb_field_type = c::bch_sb_field_type::extent_type_u64s;
}

