// Auto-generated from BCH_EXTENT_ENTRY_TYPES() — do not edit

/// Size in u64s for each known extent entry type.
pub fn extent_entry_type_u64s(ty: u32) -> Option<usize> {
    use core::mem::size_of;
    Some(match ty {
        0 => size_of::<c::bch_extent_ptr>() / 8,
        1 => size_of::<c::bch_extent_crc32>() / 8,
        2 => size_of::<c::bch_extent_crc64>() / 8,
        3 => size_of::<c::bch_extent_crc128>() / 8,
        4 => size_of::<c::bch_extent_stripe_ptr>() / 8,
        5 => size_of::<c::bch_extent_rebalance_v1>() / 8,
        6 => size_of::<c::bch_extent_flags>() / 8,
        7 => size_of::<c::bch_extent_reconcile>() / 8,
        8 => size_of::<c::bch_extent_reconcile_bp>() / 8,
        _ => return None,
    })
}
