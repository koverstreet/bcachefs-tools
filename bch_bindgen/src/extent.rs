//! Extent entry and pointer iterators.
//!
//! Extent-type bkeys (btree_ptr, extent, reflink_v, stripe, btree_ptr_v2)
//! store a heterogeneous array of entries in their values: pointers, CRC
//! checksums, stripe pointers, flags, etc. Each entry type is tagged with
//! a bit-position encoding in its low bits — the type is recovered by
//! finding the first set bit (equivalent to C's `__ffs`).
//!
//! This module provides safe Rust iterators over these entries, equivalent
//! to the C macros `bkey_extent_entry_for_each` and `bkey_for_each_ptr`.
//!
//! # Safety notes
//!
//! The iterators walk raw pointers within the bkey value region. Safety
//! depends on:
//! - The bkey_i having a valid `k.u64s` field (determines value bounds)
//! - The extent entries being well-formed (correct type tags and sizes)
//!
//! These invariants hold for any bkey read from disk or the btree — the
//! on-disk format guarantees them, and fsck validates them.

use crate::c;
use std::marker::PhantomData;

include!(concat!(env!("OUT_DIR"), "/extent_entry_types_gen.rs"));

/// Size of `struct bkey` in u64s (40 bytes).
const BKEY_U64S: usize = std::mem::size_of::<c::bkey>() / 8;

/// Extract the extent entry type from its bit-position encoding.
///
/// Each entry type stores `1 << type_enum` in the low bits of the first
/// word. This function recovers the enum value via `trailing_zeros`,
/// equivalent to the C `extent_entry_type()` which uses `__ffs`.
///
/// Returns `u32::MAX` for an invalid (zero) type field.
pub fn extent_entry_type(entry: &c::bch_extent_entry) -> u32 {
    // SAFETY: reading the type_ field of the union, which overlaps all members
    let t = unsafe { entry.type_ } as u64;
    if t != 0 { t.trailing_zeros() } else { u32::MAX }
}

/// Iterator over extent entries in a bkey value.
///
/// Yields references to each `bch_extent_entry` in order. The caller can
/// inspect the type with [`extent_entry_type`] and access the appropriate
/// union member.
pub struct ExtentEntryIter<'a> {
    cur: *const u64,
    end: *const u64,
    _marker: PhantomData<&'a c::bkey_i>,
}

impl<'a> Iterator for ExtentEntryIter<'a> {
    type Item = &'a c::bch_extent_entry;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cur >= self.end {
            return None;
        }

        // SAFETY: cur is within the bkey value region, which is valid for reads
        let entry = unsafe { &*(self.cur as *const c::bch_extent_entry) };
        let ty = extent_entry_type(entry);
        let size = extent_entry_type_u64s(ty)?;
        if size == 0 {
            return None;
        }

        // SAFETY: advancing within the bounds checked by the cur < end test above
        self.cur = unsafe { self.cur.add(size) };
        Some(entry)
    }
}

/// Iterator over extent pointers in a bkey value.
///
/// Filters [`ExtentEntryIter`] to yield only `bch_extent_ptr` entries,
/// equivalent to the C `bkey_for_each_ptr` macro.
pub struct ExtentPtrIter<'a> {
    inner: ExtentEntryIter<'a>,
}

impl<'a> Iterator for ExtentPtrIter<'a> {
    type Item = &'a c::bch_extent_ptr;

    fn next(&mut self) -> Option<Self::Item> {
        for entry in self.inner.by_ref() {
            if extent_entry_type(entry) == 0 {
                // SAFETY: type 0 = BCH_EXTENT_ENTRY_ptr, so the ptr union member is valid
                return Some(unsafe { &entry.ptr });
            }
        }
        None
    }
}

/// Iterate over all extent entries in a bkey.
///
/// Returns an empty iterator for key types that don't have extent entries.
pub fn bkey_extent_entries(k: &c::bkey_i) -> ExtentEntryIter<'_> {
    let (start, end) = extent_entry_range(k);
    ExtentEntryIter { cur: start, end, _marker: PhantomData }
}

/// Iterate over extent pointers in a bkey.
///
/// Returns an empty iterator for key types that don't have extent entries.
pub fn bkey_extent_ptrs(k: &c::bkey_i) -> ExtentPtrIter<'_> {
    ExtentPtrIter { inner: bkey_extent_entries(k) }
}

/// Compute the (start, end) u64 pointers for the extent entry region of a bkey.
///
/// Dispatches on key type, matching the C `bch2_bkey_ptrs_c()` function.
fn extent_entry_range(k: &c::bkey_i) -> (*const u64, *const u64) {
    use c::bch_bkey_type::*;

    let val = &k.v as *const c::bch_val as *const u64;
    let val_u64s = (k.k.u64s as usize).saturating_sub(BKEY_U64S);
    let val_end = unsafe { val.add(val_u64s) };

    let empty = (val_end, val_end);
    if val_u64s == 0 {
        return empty;
    }

    let t = k.k.type_ as u32;
    match () {
        // btree_ptr / extent: entries start immediately at the value
        _ if t == KEY_TYPE_btree_ptr as u32 || t == KEY_TYPE_extent as u32 => {
            (val, val_end)
        }

        // btree_ptr_v2: 40 bytes (5 u64s) of fixed header before entries
        _ if t == KEY_TYPE_btree_ptr_v2 as u32 => {
            let header = std::mem::size_of::<c::bch_btree_ptr_v2>() / 8;
            if val_u64s <= header {
                return empty;
            }
            (unsafe { val.add(header) }, val_end)
        }

        // reflink_v: 8 bytes (1 u64) refcount before entries
        _ if t == KEY_TYPE_reflink_v as u32 => {
            let header = std::mem::size_of::<c::bch_reflink_v>() / 8;
            if val_u64s <= header {
                return empty;
            }
            (unsafe { val.add(header) }, val_end)
        }

        // stripe: 8 bytes (1 u64) fixed header, then ptrs[nr_blocks]
        _ if t == KEY_TYPE_stripe as u32 => {
            let header = std::mem::size_of::<c::bch_stripe>() / 8;
            if val_u64s <= header {
                return empty;
            }
            // Cast to bch_stripe to read nr_blocks safely
            let stripe = unsafe { &*(val as *const c::bch_stripe) };
            let nr_blocks = stripe.nr_blocks as usize;
            let ptr_u64s = std::mem::size_of::<c::bch_extent_ptr>() / 8;
            let ptrs_start = unsafe { val.add(header) };
            let ptrs_end_u64s = header + nr_blocks * ptr_u64s;
            let ptrs_end = if ptrs_end_u64s <= val_u64s {
                unsafe { val.add(ptrs_end_u64s) }
            } else {
                val_end
            };
            (ptrs_start, ptrs_end)
        }

        _ => empty,
    }
}
