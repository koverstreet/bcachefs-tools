use crate::btree::bkey::{BkeyS, BkeySC};
use crate::btree::iter::{BtreeIter, TransAttempt};
use crate::errcode::BchError;
use crate::c;
use crate::c::{BchExtentEntryRef as Entry, ExtentEntryType};
use crate::fs::Fs;
use crate::types::ArmOf;
use core::mem::{align_of, size_of};

macro_rules! extent_entry_u64s {
    ($(($name:tt, $nr:literal)),* $(,)?) => { ::paste::paste! {
        /// Size in u64s for each known extent entry type.
        pub fn extent_entry_type_u64s(ty: u32) -> Option<usize> {
            Some(match ty {
                $($nr => size_of::<c::[<bch_extent_ $name>]>() / 8,)*
                _ => return None,
            })
        }
    } };
}
c::BCH_EXTENT_ENTRY_TYPES!(extent_entry_u64s);

/// A key's extent entries, each the arm its type says, bounded by its own
/// bytes. Stops at a type this code doesn't know, or past the value's end.
pub struct ExtentEntryIter<'a> {
    b: &'a [u8],
}

impl<'a> Iterator for ExtentEntryIter<'a> {
    type Item = Entry<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let ty = ExtentEntryType::of_bytes(self.b)?;
        let (entry, rest) = self.b.split_at_checked(extent_entry_type_u64s(ty)? * 8)?;
        self.b = rest;
        c::bch_extent_entry::arm_at(ty, entry)
    }
}

/// Iterate over all extent entries in a bkey.
///
/// Returns an empty iterator for key types that don't have extent entries.
pub fn bkey_extent_entries_sc(k: BkeySC<'_>) -> ExtentEntryIter<'_> {
    ExtentEntryIter { b: k.extent_entry_bytes() }
}

/// Iterate over all extent entries in a `bkey_i`.
pub fn bkey_extent_entries(k: &c::bkey_i) -> ExtentEntryIter<'_> {
    bkey_extent_entries_sc(k.into())
}

/// Iterate over extent pointers in a bkey, skipping non-pointer entries.
pub fn bkey_ptrs_sc(k: BkeySC<'_>) -> impl Iterator<Item = &c::bch_extent_ptr> {
    bkey_extent_entries_sc(k).filter_map(|e| match e {
        Entry::ptr(p) => Some(p.head()),
        _ => None,
    })
}

/// Iterate over extent pointers in a `bkey_i`.
pub fn bkey_ptrs(k: &c::bkey_i) -> impl Iterator<Item = &c::bch_extent_ptr> {
    bkey_ptrs_sc(k.into())
}

/// A key's extent entry, mutably: its type, and its bytes.
pub struct ExtentEntryMut<'a> {
    ty: u32,
    b:  &'a mut [u8],
}

impl<'a> ExtentEntryMut<'a> {
    /// The entry as arm @A, if that's its type.
    pub fn into_arm_mut<A: ArmOf<c::bch_extent_entry, Tag = u32>>(self) -> Option<&'a mut A> {
        let fits = self.b.len() >= size_of::<A>() && self.b.as_ptr() as usize % align_of::<A>() == 0;
        // SAFETY: the type says these bytes are an A, and an A fits there,
        // aligned; an arm is plain data. Writing its type field through it
        // makes them another arm - still plain data, read by type each time.
        (self.ty == A::TAG && fits).then(|| unsafe { &mut *self.b.as_mut_ptr().cast::<A>() })
    }
}

/// A key's extent entries, mutably - each sized by the filesystem's table,
/// so a type this code doesn't know is stepped over.
pub struct ExtentEntryIterMut<'a> {
    fs: &'a Fs,
    b:  &'a mut [u8],
}

impl<'a> Iterator for ExtentEntryIterMut<'a> {
    type Item = ExtentEntryMut<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let ty = ExtentEntryType::of_bytes(self.b)?;
        let bytes = entry_u64s(self.fs, ty) * 8;
        let (b, rest) = core::mem::take(&mut self.b).split_at_mut_checked(bytes).filter(|_| bytes > 0)?;
        self.b = rest;
        Some(ExtentEntryMut { ty, b })
    }
}

pub(crate) fn bkey_extent_entries_mut<'a>(
    fs: &'a Fs,
    k:  &'a mut BkeyS<'_>,
) -> ExtentEntryIterMut<'a> {
    ExtentEntryIterMut { fs, b: k.extent_entry_bytes_mut() }
}

/// The size in u64s of an entry of type @ty, from this filesystem's table -
/// which can know types this code doesn't: as extent_entry_u64s().
fn entry_u64s(fs: &Fs, ty: u32) -> usize {
    let ty = ty as usize;
    let sb = unsafe { &(*fs.raw).sb };
    assert!(ty < sb.extent_types_known as usize,
            "extent entry type {ty}, of {} known: a validated key has none", sb.extent_types_known);
    sb.extent_type_u64s[ty] as usize
}

pub fn bkey_ptrs_mut<'a>(
    fs: &'a Fs,
    k:  &'a mut BkeyS<'_>,
) -> impl Iterator<Item = &'a mut c::bch_extent_ptr> {
    bkey_extent_entries_mut(fs, k).filter_map(|e| e.into_arm_mut())
}

/// The checksum/compression entries of @k, unpacked: as bkey_for_each_crc().
pub fn bkey_crcs<'a>(k: BkeySC<'a>) -> impl Iterator<Item = c::bch_extent_crc_unpacked> + 'a {
    bkey_extent_entries_sc(k).filter_map(move |e| {
        let crc: *const u8 = match e {
            Entry::crc32(crc)  => crc.head() as *const _ as _,
            Entry::crc64(crc)  => crc.head() as *const _ as _,
            Entry::crc128(crc) => crc.head() as *const _ as _,
            _ => return None,
        };
        // SAFETY: a crc entry, which C reads by its type
        Some(unsafe { c::bch2_extent_crc_unpack(k.k, crc.cast()) })
    })
}

/// Whether @crc's data is checksummed or compressed - read back whole, so
/// bounded by encoded_extent_max: as crc_is_encoded().
pub fn crc_is_encoded(crc: &c::bch_extent_crc_unpacked) -> bool {
    // By value in C; bindgen doesn't make it Copy
    unsafe { c::crc_is_encoded(core::ptr::read(crc)) }
}

/// Whether @k is counted in its inode's i_sectors: as
/// bkey_extent_is_allocation().
pub fn bkey_extent_is_allocation(k: &c::bkey) -> bool {
    use c::bch_bkey_type as t;

    [t::KEY_TYPE_extent, t::KEY_TYPE_reservation, t::KEY_TYPE_reflink_p, t::KEY_TYPE_reflink_v,
     t::KEY_TYPE_inline_data, t::KEY_TYPE_indirect_inline_data, t::KEY_TYPE_error]
        .iter().any(|ty| ty.0 == k.type_ as u32)
}

/// Whether @k reserves space rather than holding data - a reservation, or
/// an extent with unwritten pointers: as bkey_extent_is_reservation().
pub fn bkey_extent_is_reservation(k: BkeySC<'_>) -> bool {
    k.k.type_ as u32 == c::bch_bkey_type::KEY_TYPE_reservation.0 ||
        bkey_ptrs_sc(k).any(|p| p.unwritten() != 0)
}

/// @k's durability, for reserving space to rewrite it: as
/// bch2_bkey_durability_safe(), which tolerates bad pointers.
pub fn durability_safe(fs: &Fs, k: BkeySC<'_>) -> c::bkey_durability {
    unsafe { c::bch2_bkey_durability_safe(fs.raw, k.to_raw()) }
}

/// Drop @k's stale cached pointers, updating it through @iter: as
/// bch2_bkey_drop_stale_ptrs().
pub fn drop_stale_ptrs<'a, 't>(
    t:    &TransAttempt<'a, 't>,
    iter: &BtreeIter<'t>,
    k:    BkeySC<'_>,
) -> Result<(), BchError> {
    let ret = unsafe { c::bch2_bkey_drop_stale_ptrs(t.raw(), iter.raw(), k.to_raw()) };
    t.result(ret)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An entry's first word: the type field for @ty - its low @ty + 1 bits,
    /// the top one set - under @bits.
    fn word(ty: u32, bits: u64) -> u64 {
        (bits << (ty + 1)) | (1 << ty)
    }

    fn bytes(words: &mut [u64]) -> &mut [u8] {
        // SAFETY: u64s are plain data
        unsafe { core::slice::from_raw_parts_mut(words.as_mut_ptr().cast(), words.len() * 8) }
    }

    /// The type of each entry in @words, and where it is, in u64s.
    fn walk(mut words: std::vec::Vec<u64>) -> std::vec::Vec<(u32, usize)> {
        let start = words.as_ptr() as usize;
        let at = |p: *const u8| (p as usize - start) / 8;
        ExtentEntryIter { b: bytes(&mut words) }
            .map(|e| match e {
                Entry::ptr(a)        => (0, at(a.head() as *const _ as _)),
                Entry::crc32(a)      => (1, at(a.head() as *const _ as _)),
                Entry::crc64(a)      => (2, at(a.head() as *const _ as _)),
                Entry::crc128(a)     => (3, at(a.head() as *const _ as _)),
                Entry::stripe_ptr(a) => (4, at(a.head() as *const _ as _)),
                _ => panic!("an entry type the test doesn't use"),
            })
            .collect()
    }

    #[test]
    fn extent_entries_each_as_long_as_its_arm() {
        // crc32, ptr, ptr, crc64, ptr, stripe_ptr, crc128, and a ptr at the
        // very end - shorter than a bch_extent_entry
        assert_eq!(walk(vec![word(1, 5), word(0, 6), word(0, 7), word(2, 8), 0, word(0, 9),
                             word(4, 10), word(3, 11), 0, 0, word(0, 12)]),
                   [(1, 0), (0, 1), (0, 2), (2, 3), (0, 5), (4, 6), (3, 7), (0, 10)]);

        // stopping at a type this code doesn't know, a crc128 two u64s long,
        // and a type word of zero
        assert_eq!(walk(vec![word(0, 1), word(20, 0), word(0, 1)]), [(0, 0)]);
        assert_eq!(walk(vec![word(0, 1), word(3, 0), 0]), [(0, 0)]);
        assert_eq!(walk(vec![word(0, 1), 0, word(0, 1)]), [(0, 0)]);
    }

    #[test]
    fn extent_entry_arm_by_type() {
        let mut w = [word(4, 3)];
        fn e(ty: u32, w: &mut [u64]) -> ExtentEntryMut<'_> {
            ExtentEntryMut { ty, b: bytes(w) }
        }
        assert!(e(4, &mut w).into_arm_mut::<c::bch_extent_ptr>().is_none(), "not its type");
        assert!(e(4, &mut w).into_arm_mut::<c::bch_extent_stripe_ptr>().is_some());
        assert!(e(3, &mut w).into_arm_mut::<c::bch_extent_crc128>().is_none(), "doesn't fit");

        // from_arm() sets the type field, whatever the arm had
        let e = c::bch_extent_entry::from_arm(c::bch_extent_crc128::default());
        assert_eq!(e.type_(), c::bch_extent_entry_type::BCH_EXTENT_ENTRY_crc128);
    }
}
