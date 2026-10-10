// SPDX-License-Identifier: GPL-2.0
//! bch_extent_entry as a tagged_union!, over bindgen's union of C's. An entry
//! is as long as its arm, not the union: read entries with arm_at() over a
//! value's bytes (data/extents.rs), not get().

use core::ffi::c_ulong;
use core::mem::size_of;

use crate::c;
use crate::types::Determinant;
use cstruct_macros::tagged_union;

/// The lowest set bit of the type word: extent_entry_type().
pub struct ExtentEntryType;

impl ExtentEntryType {
    /// C's `type`: the second word on 32-bit big endian.
    const WORD: usize =
        if cfg!(all(target_endian = "big", target_pointer_width = "32")) { size_of::<c_ulong>() } else { 0 };

    /// The tag of the entry @b starts with - u32::MAX if its type word is 0.
    pub fn of_bytes(b: &[u8]) -> Option<u32> {
        let w = c_ulong::from_ne_bytes(b.get(Self::WORD..)?.get(..size_of::<c_ulong>())?.try_into().ok()?);
        Some(if w != 0 { w.trailing_zeros() } else { u32::MAX })
    }
}

impl Determinant<c::bch_extent_entry> for ExtentEntryType {
    type Tag = u32;

    fn get(u: &c::bch_extent_entry) -> u32 {
        Self::of_bytes(u.as_bytes()).unwrap()
    }

    /// An arm's type field is its low @tag + 1 bits, the top one set.
    fn set(u: &mut c::bch_extent_entry, tag: u32) {
        // SAFETY: plain data, inside the storage
        unsafe {
            let p = (u as *mut c::bch_extent_entry).cast::<u8>().add(Self::WORD).cast::<c_ulong>();
            p.write_unaligned((p.read_unaligned() & !((2 << tag) - 1)) | (1 << tag));
        }
    }
}

tagged_union! {
    pub union bch_extent_entry in c::bch_extent_entry {
        tag type_: u32 = c::bch_extent_entry_type by ExtentEntryType,
        arms from BCH_EXTENT_ENTRY_TYPES(f, n) => f: c::bch_extent_ ## f = n,
    }
}
