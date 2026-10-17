//! A key's extent entries - its pointers, and what applies to them - as the
//! arms they are.
//!
//! An entry's type is the lowest set bit of its first word; how long a type
//! newer than this code is, only the filesystem's table says (sb
//! extent_type_u64s). So a walk steps by that table, and hands out a type
//! this code doesn't know as Unknown, its bytes - validate passes those.
//! Where the walk can't step - a type the filesystem doesn't know, or past
//! the value's end - the rest of the value is one last Unknown entry, so
//! printing a damaged key shows the damage.

use crate::btree::bkey::{BkeyS, BkeySC};
use crate::btree::iter::{BtreeIter, TransAttempt};
use crate::errcode::BchError;
use crate::c;
use crate::fs::Fs;
use crate::types::Le64;
use crate::util::vstructs::{cast_plain, cast_plain_mut};
use core::mem::size_of;

macro_rules! extent_entries {
    ($(($name:tt, $nr:literal)),* $(,)?) => { ::paste::paste! {
        pub fn extent_entry_type_u64s(ty: u32) -> Option<usize> {
            Some(match ty {
                $($nr => size_of::<c::[<bch_extent_ $name>]>() / 8,)*
                _ => return None,
            })
        }

        #[allow(non_camel_case_types)]
        #[derive(Clone, Copy)]
        pub enum ExtentEntry<'a> {
            $($name(&'a c::[<bch_extent_ $name>]),)*
            /// One of a type this code doesn't know.
            Unknown { ty: u32, bytes: &'a [u8] },
        }

        /// An Unknown entry can't be edited.
        #[allow(non_camel_case_types)]
        pub enum ExtentEntryMut<'a> {
            $($name(&'a mut c::[<bch_extent_ $name>]),)*
            Unknown { ty: u32, bytes: &'a [u8] },
        }

        impl<'a> ExtentEntry<'a> {
            /// Unknown unless @b is exactly an arm of type @ty.
            fn new(ty: u32, b: &'a [u8]) -> Self {
                match ty {
                    $($nr if b.len() == size_of::<c::[<bch_extent_ $name>]>() =>
                        if let Some(a) = cast_plain(b) { return Self::$name(a) },)*
                    _ => {}
                }
                Self::Unknown { ty, bytes: b }
            }
        }

        impl<'a> ExtentEntryMut<'a> {
            fn new(ty: u32, b: &'a mut [u8]) -> Self {
                match ty {
                    $($nr if b.len() == size_of::<c::[<bch_extent_ $name>]>() &&
                             cast_plain::<c::[<bch_extent_ $name>]>(b).is_some() =>
                        return Self::$name(cast_plain_mut(b).unwrap()),)*
                    _ => {}
                }
                Self::Unknown { ty, bytes: b }
            }
        }
    } };
}
c::BCH_EXTENT_ENTRY_TYPES!(extent_entries);

/// The lowest set bit of @b's first word; u32::MAX if it has none.
fn entry_type(b: &[u8]) -> Option<u32> {
    let w = u64::from_ne_bytes(b.get(..8)?.try_into().ok()?);
    Some(if w != 0 { w.trailing_zeros() } else { u32::MAX })
}

/// The filesystem's entry lengths in u64s, which know types newer than this
/// code. A copy - the table is set once the filesystem's up - so a walk
/// borrows only its key.
#[derive(Clone, Copy)]
struct EntrySizes {
    u64s: [u8; 31],
    nr:   u8,
}

impl EntrySizes {
    fn of(fs: &Fs) -> Self {
        // SAFETY: the filesystem's in-memory superblock info, live as long as it is
        let sb = unsafe { &(*fs.as_ptr()).sb };
        EntrySizes { u64s: sb.extent_type_u64s, nr: sb.extent_types_known }
    }

    /// An entry of type @ty's bytes: None for a type it doesn't know.
    fn bytes(&self, ty: u32) -> Option<usize> {
        let u64s = *self.u64s[..usize::from(self.nr).min(self.u64s.len())].get(ty as usize)?;
        (u64s != 0).then_some(usize::from(u64s) * 8)
    }
}

/// The type and length of the entry @b starts with, as @sizes has it - or,
/// if it can't step there, @b's: None at the end.
fn entry_len(sizes: &EntrySizes, b: &[u8]) -> Option<(u32, usize)> {
    if b.is_empty() {
        return None;
    }
    let ty = entry_type(b).unwrap_or(u32::MAX);
    let len = sizes.bytes(ty).filter(|len| *len <= b.len()).unwrap_or(b.len());
    Some((ty, len))
}

struct Entries<'a> {
    sizes: EntrySizes,
    b:     &'a [u8],
}

impl<'a> Iterator for Entries<'a> {
    type Item = (u32, &'a [u8]);

    fn next(&mut self) -> Option<(u32, &'a [u8])> {
        let (ty, len) = entry_len(&self.sizes, self.b)?;
        let (e, rest) = self.b.split_at(len);
        self.b = rest;
        Some((ty, e))
    }
}

struct EntriesMut<'a> {
    sizes: EntrySizes,
    b:     &'a mut [u8],
}

impl<'a> Iterator for EntriesMut<'a> {
    type Item = (u32, &'a mut [u8]);

    fn next(&mut self) -> Option<(u32, &'a mut [u8])> {
        let (ty, len) = entry_len(&self.sizes, self.b)?;
        let (e, rest) = core::mem::take(&mut self.b).split_at_mut(len);
        self.b = rest;
        Some((ty, e))
    }
}

impl<'a> BkeySC<'a> {
    fn entries(self, fs: &Fs) -> Entries<'a> {
        Entries { sizes: EntrySizes::of(fs), b: self.extent_entry_bytes() }
    }

    /// None, for a key without.
    pub fn extent_entries(self, fs: &Fs) -> impl Iterator<Item = ExtentEntry<'a>> {
        self.entries(fs).map(|(ty, b)| ExtentEntry::new(ty, b))
    }

    pub fn ptrs(self, fs: &Fs) -> impl Iterator<Item = &'a c::bch_extent_ptr> {
        self.extent_entries(fs).filter_map(|e| match e {
            ExtentEntry::ptr(p) => Some(p),
            _ => None,
        })
    }

    pub fn crcs(self, fs: &Fs) -> impl Iterator<Item = c::bch_extent_crc_unpacked> + 'a {
        self.extent_entries(fs).filter_map(move |e| crc_unpack(self.k, e))
    }
}

impl BkeyS<'_> {
    pub fn extent_entries_mut(&mut self, fs: &Fs) -> impl Iterator<Item = ExtentEntryMut<'_>> {
        EntriesMut { sizes: EntrySizes::of(fs), b: self.extent_entry_bytes_mut() }
            .map(|(ty, b)| ExtentEntryMut::new(ty, b))
    }

    pub fn ptrs_mut(&mut self, fs: &Fs) -> impl Iterator<Item = &mut c::bch_extent_ptr> {
        self.extent_entries_mut(fs).filter_map(|e| match e {
            ExtentEntryMut::ptr(p) => Some(p),
            _ => None,
        })
    }
}

/// @entry, if it's a crc entry of @k, unpacked: the sizes unbiased, the
/// checksum's bytes where bch_csum has them.
fn crc_unpack(k: &c::bkey, entry: ExtentEntry<'_>) -> Option<c::bch_extent_crc_unpacked> {
    // The packed checksum bytes, at the start of a bch_csum word - C stores
    // them there through a cast, with no byte swap:
    fn csum_word(bytes: &[u8]) -> Le64 {
        let mut w = [0u8; 8];
        w[..bytes.len()].copy_from_slice(bytes);
        Le64::new(u64::from_le_bytes(w))
    }

    let unpacked = |csum_type: u8, compression_type: u8, compressed_raw: u16,
                    uncompressed_raw: u16, offset: u16, nonce: u16, csum| {
        c::bch_extent_crc_unpacked {
            compressed_size:   u32::from(compressed_raw) + 1,
            uncompressed_size: u32::from(uncompressed_raw) + 1,
            live_size:         k.size,
            csum_type,
            compression_type,
            offset,
            nonce,
            csum,
        }
    };

    Some(match entry {
        ExtentEntry::crc32(e) =>
            unpacked(e.csum_type(), e.compression_type(),
                     e.compressed_size_raw().into(), e.uncompressed_size_raw().into(),
                     e.offset().into(), 0,
                     c::bch_csum { lo: csum_word(&e.csum().to_ne_bytes()), hi: Le64::new(0) }),
        ExtentEntry::crc64(e) =>
            unpacked(e.csum_type(), e.compression_type(),
                     e.compressed_size_raw(), e.uncompressed_size_raw(),
                     e.offset(), e.nonce(),
                     c::bch_csum { lo: csum_word(&{ e.csum_lo }.to_ne_bytes()),
                                   hi: csum_word(&e.csum_hi().to_ne_bytes()) }),
        ExtentEntry::crc128(e) =>
            unpacked(e.csum_type(), e.compression_type(),
                     e.compressed_size_raw(), e.uncompressed_size_raw(),
                     e.offset(), e.nonce(), e.csum),
        _ => return None,
    })
}

/// Whether @crc's data is checksummed or compressed - read back whole, so
/// bounded by encoded_extent_max.
pub fn crc_is_encoded(crc: &c::bch_extent_crc_unpacked) -> bool {
    let compressed = crc.compression_type as u32 != c::bch_compression_type::BCH_COMPRESSION_TYPE_none.0 &&
        crc.compression_type as u32 != c::bch_compression_type::BCH_COMPRESSION_TYPE_incompressible.0;

    crc.csum_type as u32 != c::bch_csum_type::BCH_CSUM_none.0 || compressed
}

impl c::bkey {
    /// Whether it's counted in its inode's i_sectors.
    pub fn extent_is_allocation(&self) -> bool {
        use c::bch_bkey_type as t;

        matches!(self.key_type(),
                 t::KEY_TYPE_extent | t::KEY_TYPE_reservation | t::KEY_TYPE_reflink_p |
                 t::KEY_TYPE_reflink_v | t::KEY_TYPE_inline_data |
                 t::KEY_TYPE_indirect_inline_data | t::KEY_TYPE_error)
    }

    /// Whether its value is pointers to its data, and not to another key's.
    pub fn extent_is_direct_data(&self) -> bool {
        use c::bch_bkey_type as t;

        matches!(self.key_type(),
                 t::KEY_TYPE_btree_ptr | t::KEY_TYPE_btree_ptr_v2 | t::KEY_TYPE_extent |
                 t::KEY_TYPE_reflink_v)
    }
}

impl BkeySC<'_> {
    /// Whether it has space allocated and not yet written: an unwritten
    /// pointer.
    pub fn extent_is_unwritten(self, fs: &Fs) -> bool {
        self.ptrs(fs).any(|p| p.unwritten() != 0)
    }

    /// Whether it reserves space rather than holding data - a reservation, or
    /// an extent with unwritten pointers.
    pub fn extent_is_reservation(self, fs: &Fs) -> bool {
        self.k.key_type() == c::bch_bkey_type::KEY_TYPE_reservation || self.extent_is_unwritten(fs)
    }

    /// For reserving space to rewrite it: tolerates bad pointers.
    pub fn durability_safe(self, fs: &Fs) -> c::bkey_durability {
        unsafe { c::bch2_bkey_durability_safe(fs.as_ptr(), self.to_raw()) }
    }
}

impl<V: ?Sized> BkeyS<'_, V> {
    /// Cut it to start at @where_: bch2_cut_front_s().
    pub fn cut_front(&mut self, fs: &Fs, where_: c::bpos) {
        unsafe { c::bch2_cut_front_s(fs.as_ptr(), where_, self.as_raw()) };
    }

    /// Cut it to end at @where_: bch2_cut_back_s().
    pub fn cut_back(&mut self, where_: c::bpos) {
        unsafe { c::bch2_cut_back_s(where_, self.as_raw()) };
    }
}

/// Drop @k's stale cached pointers, updating it through @iter.
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

    /// Our sizes, as a filesystem's table has them for the types we know.
    fn ours() -> std::vec::Vec<u8> {
        (0..c::BCH_EXTENT_ENTRY_MAX).map(|ty| extent_entry_type_u64s(ty).unwrap() as u8).collect()
    }

    /// walk()'s type for an Unknown entry.
    const UNKNOWN: u32 = u32::MAX;

    /// The type of each entry in @words, and where it is, in u64s: walked by
    /// @table, as a filesystem's.
    fn walk(table: &[u8], mut words: std::vec::Vec<u64>) -> std::vec::Vec<(u32, usize)> {
        let mut sizes = EntrySizes { u64s: [0; 31], nr: table.len() as u8 };
        sizes.u64s[..table.len()].copy_from_slice(table);
        let start = words.as_ptr() as usize;
        let at = |p: *const u8| (p as usize - start) / 8;
        Entries { sizes, b: bytes(&mut words) }
            .map(|(ty, b)| ExtentEntry::new(ty, b))
            .map(|e| match e {
                ExtentEntry::ptr(a)        => (0, at(a as *const _ as _)),
                ExtentEntry::crc32(a)      => (1, at(a as *const _ as _)),
                ExtentEntry::crc64(a)      => (2, at(a as *const _ as _)),
                ExtentEntry::crc128(a)     => (3, at(a as *const _ as _)),
                ExtentEntry::stripe_ptr(a) => (4, at(a as *const _ as _)),
                ExtentEntry::Unknown { bytes, .. } => (UNKNOWN, at(bytes.as_ptr())),
                _ => panic!("an entry type the test doesn't use"),
            })
            .collect()
    }

    #[test]
    fn extent_entries_each_as_long_as_its_arm() {
        // crc32, ptr, ptr, crc64, ptr, stripe_ptr, crc128, and a ptr at the
        // very end - shorter than a bch_extent_entry
        assert_eq!(walk(&ours(), vec![word(1, 5), word(0, 6), word(0, 7), word(2, 8), 0, word(0, 9),
                                      word(4, 10), word(3, 11), 0, 0, word(0, 12)]),
                   [(1, 0), (0, 1), (0, 2), (2, 3), (0, 5), (4, 6), (3, 7), (0, 10)]);

        // where it can't step - a type the filesystem doesn't know, a crc128
        // two u64s long, a type word of zero - the rest of the value
        assert_eq!(walk(&ours(), vec![word(0, 1), word(20, 0), word(0, 1)]), [(0, 0), (UNKNOWN, 1)]);
        assert_eq!(walk(&ours(), vec![word(0, 1), word(3, 0), 0]), [(0, 0), (UNKNOWN, 1)]);
        assert_eq!(walk(&ours(), vec![word(0, 1), 0, word(0, 1)]), [(0, 0), (UNKNOWN, 1)]);

        // stepping over one only the filesystem knows, by its table's size
        let mut newer = ours();
        newer.resize(20, 0);
        newer.push(1);
        assert_eq!(walk(&newer, vec![word(0, 1), word(20, 0), word(0, 1)]),
                   [(0, 0), (UNKNOWN, 1), (0, 2)]);
    }

    #[test]
    fn extent_entry_arm_by_type() {
        let mut w = [word(4, 3)];
        assert!(matches!(ExtentEntryMut::new(4, bytes(&mut w)), ExtentEntryMut::stripe_ptr(_)));
        assert!(matches!(ExtentEntryMut::new(3, bytes(&mut w)), ExtentEntryMut::Unknown { ty: 3, .. }),
                "a crc128 doesn't fit in a word");
    }
}
