//! Keys with pointers - btree node pointers, extents, indirect extents,
//! stripes: the key types BCH_BKEY_TYPES() flags BKEY_TYPE_ptrs. A key
//! narrows to one with arm::<Ptrs>() - BkeySC<'_, Ptrs> and the rest, the
//! same keys with the other arms dropped; each value type says where its
//! extent entries are (PtrsBkey), and the entries - its pointers, and what
//! applies to them - are walked as the arms they are (BkeyVPtrs).
//!
//! An entry's type is the lowest set bit of its first word; how long a type
//! newer than this code is, only the filesystem's table says (sb
//! extent_type_u64s). So a walk steps by that table, and hands out a type
//! this code doesn't know as Unknown, its bytes - validate passes those.
//! Where the walk can't step, the rest of the value is one last entry, so
//! printing a damaged key shows the damage: Invalid, of a type the filesystem
//! doesn't know, or PastEnd, one that runs past the value's end - which
//! validate reports as different errors, and to_text prints differently.

use crate::btree::bkey::{bkey_family, BkeySC, BkeyV, BkeyVal, BkeyVMut};
use crate::btree::iter::{BtreeIter, TransAttempt};
use crate::errcode::BchError;
use crate::c;
use crate::fs::Fs;
use crate::types::Le64;
use crate::util::vstructs::{cast_plain, cast_plain_mut, ArmRef, Validated};
use core::mem::{offset_of, size_of};
use core::ops::Range;

macro_rules! extent_entries {
    ($(($name:tt, $nr:literal)),* $(,)?) => { ::paste::paste! {
        pub fn extent_entry_type_u64s(ty: u32) -> Option<usize> {
            Some(match ty {
                $($nr => size_of::<c::[<bch_extent_ $name>]>() / 8,)*
                _ => return None,
            })
        }

        pub fn extent_entry_type_name(ty: u32) -> Option<&'static str> {
            Some(match ty {
                $($nr => stringify!($name),)*
                _ => return None,
            })
        }

        #[allow(non_camel_case_types)]
        #[derive(Clone, Copy)]
        pub enum ExtentEntry<'a> {
            $($name(&'a c::[<bch_extent_ $name>]),)*
            /// One of a type this code doesn't know, the filesystem's table
            /// stepping over it.
            Unknown { ty: u32, bytes: &'a [u8] },
            /// One of a type the filesystem doesn't know: the rest of the value.
            Invalid { ty: u32, bytes: &'a [u8] },
            /// One that runs past the value's end: the rest of the value.
            PastEnd { ty: u32, bytes: &'a [u8] },
        }

        /// An Unknown, Invalid or PastEnd entry can't be edited.
        #[allow(non_camel_case_types)]
        pub enum ExtentEntryMut<'a> {
            $($name(&'a mut c::[<bch_extent_ $name>]),)*
            Unknown { ty: u32, bytes: &'a [u8] },
            Invalid { ty: u32, bytes: &'a [u8] },
            PastEnd { ty: u32, bytes: &'a [u8] },
        }

        impl ExtentEntry<'_> {
            /// Its type: extent_entry_type().
            pub fn ty(&self) -> u32 {
                match self {
                    $(Self::$name(_) => $nr,)*
                    Self::Unknown { ty, .. } | Self::Invalid { ty, .. } | Self::PastEnd { ty, .. } => *ty,
                }
            }
        }

        impl<'a> ExtentEntry<'a> {
            /// The entry where the walk had to @stop, or else Unknown unless
            /// @b is exactly an arm of type @ty.
            fn new(ty: u32, b: &'a [u8], stop: Option<Stop>) -> Self {
                match stop {
                    Some(Stop::Invalid) => return Self::Invalid { ty, bytes: b },
                    Some(Stop::PastEnd) => return Self::PastEnd { ty, bytes: b },
                    None => {}
                }
                match ty {
                    $($nr if b.len() == size_of::<c::[<bch_extent_ $name>]>() =>
                        if let Some(a) = cast_plain(b) { return Self::$name(a) },)*
                    _ => {}
                }
                Self::Unknown { ty, bytes: b }
            }
        }

        impl<'a> ExtentEntryMut<'a> {
            fn new(ty: u32, b: &'a mut [u8], stop: Option<Stop>) -> Self {
                match stop {
                    Some(Stop::Invalid) => return Self::Invalid { ty, bytes: b },
                    Some(Stop::PastEnd) => return Self::PastEnd { ty, bytes: b },
                    None => {}
                }
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

/// Why a walk can't step past an entry, whose bytes are then the rest of the
/// value - in C's order of checking: bch2_bkey_ptrs_validate()'s.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stop {
    /// A type the filesystem doesn't know - so not its length either.
    Invalid,
    /// A known type's length is more than what's left.
    PastEnd,
}

/// The type and length of the entry @b starts with, as @sizes has it - or,
/// if it can't step there, @b's and why: None at the end.
fn entry_len(sizes: &EntrySizes, b: &[u8]) -> Option<(u32, usize, Option<Stop>)> {
    if b.is_empty() {
        return None;
    }
    let ty = entry_type(b).unwrap_or(u32::MAX);
    Some(match sizes.bytes(ty) {
        None                        => (ty, b.len(), Some(Stop::Invalid)),
        Some(len) if len > b.len()  => (ty, b.len(), Some(Stop::PastEnd)),
        Some(len)                   => (ty, len, None),
    })
}

struct Entries<'a> {
    sizes: EntrySizes,
    b:     &'a [u8],
}

impl<'a> Iterator for Entries<'a> {
    type Item = (u32, &'a [u8], Option<Stop>);

    fn next(&mut self) -> Option<Self::Item> {
        let (ty, len, stop) = entry_len(&self.sizes, self.b)?;
        let (e, rest) = self.b.split_at(len);
        self.b = rest;
        Some((ty, e, stop))
    }
}

struct EntriesMut<'a> {
    sizes: EntrySizes,
    b:     &'a mut [u8],
}

impl<'a> Iterator for EntriesMut<'a> {
    type Item = (u32, &'a mut [u8], Option<Stop>);

    fn next(&mut self) -> Option<Self::Item> {
        let (ty, len, stop) = entry_len(&self.sizes, self.b)?;
        let (e, rest) = core::mem::take(&mut self.b).split_at_mut(len);
        self.b = rest;
        Some((ty, e, stop))
    }
}

// ── keys with pointers ───────────────────────────────────────────────────

/// A value with pointers - of a BKEY_TYPE_ptrs type, or Ptrs, any of them:
/// where in it its extent entries are.
pub trait PtrsBkey: BkeyVal {
    fn entry_range(k: BkeySC<'_, Self>) -> Range<usize>;
}

/// The types whose extent entries run from their start[] to the value's end.
macro_rules! entries_to_end {
    ($($ty:ident),*) => { $(
        impl PtrsBkey for c::$ty<Validated> {
            fn entry_range(k: BkeySC<'_, Self>) -> Range<usize> {
                offset_of!(c::$ty, repr.start)..k.val_bytes().len()
            }
        }
    )* };
}
entries_to_end!(BtreePtr, BtreePtrV2, Extent, ReflinkV);

/// A stripe's are its ptrs[nr_blocks] - cut short at the value's end: to_text
/// and swab run before validate has checked nr_blocks against it, or that
/// there's a stripe header to read it from - none, if there isn't.
impl PtrsBkey for c::Stripe<Validated> {
    fn entry_range(k: BkeySC<'_, Self>) -> Range<usize> {
        let start = offset_of!(c::Stripe, repr.ptrs);
        let Some(s) = Self::arm_ref(k.val_bytes()) else { return start..start };
        start..(start + s.nr_blocks as usize * size_of::<c::bch_extent_ptr>()).min(k.val_bytes().len())
    }
}

bkey_family!(Ptrs);

macro_rules! ptrs_bkey {
    ($(($name:ident, $nr:literal, $($rest:tt)*)),* $(,)?) => {
        impl PtrsBkey for Ptrs {
            fn entry_range(k: BkeySC<'_, Ptrs>) -> Range<usize> {
                match k.typed() {
                    $(PtrsTyped::$name(k) => PtrsBkey::entry_range(k),)*
                }
            }
        }
    };
}
c::BCH_BKEY_TYPES!(cstruct_macros::xmacro_select [BKEY_TYPE_ptrs => ptrs_bkey]);

/// The extent entries of a key with pointers, in whichever form it's held:
/// any BkeyV whose value is a PtrsBkey - armed to Ptrs, or to one of them.
pub trait BkeyVPtrs: BkeyV<V: PtrsBkey> {
    fn extent_entries(&self, fs: &Fs) -> impl Iterator<Item = ExtentEntry<'_>> {
        let k = self.as_sc();
        // Empty for a value too short to hold them
        let b = k.val_bytes().get(<Self::V as PtrsBkey>::entry_range(k)).unwrap_or_default();
        Entries { sizes: EntrySizes::of(fs), b }.map(|(ty, b, stop)| ExtentEntry::new(ty, b, stop))
    }

    fn ptrs(&self, fs: &Fs) -> impl Iterator<Item = &c::bch_extent_ptr> {
        self.extent_entries(fs).filter_map(|e| match e {
            ExtentEntry::ptr(p) => Some(p),
            _ => None,
        })
    }

    fn crcs(&self, fs: &Fs) -> impl Iterator<Item = c::bch_extent_crc_unpacked> {
        let k = self.k();
        self.extent_entries(fs).filter_map(move |e| crc_unpack(k, e))
    }

    fn extent_entries_mut(&mut self, fs: &Fs) -> impl Iterator<Item = ExtentEntryMut<'_>>
    where Self: BkeyVMut
    {
        let range = <Self::V as PtrsBkey>::entry_range(self.as_sc());
        let b = self.as_s().into_val_bytes_mut().get_mut(range).unwrap_or_default();
        EntriesMut { sizes: EntrySizes::of(fs), b }.map(|(ty, b, stop)| ExtentEntryMut::new(ty, b, stop))
    }

    fn ptrs_mut(&mut self, fs: &Fs) -> impl Iterator<Item = &mut c::bch_extent_ptr>
    where Self: BkeyVMut
    {
        self.extent_entries_mut(fs).filter_map(|e| match e {
            ExtentEntryMut::ptr(p) => Some(p),
            _ => None,
        })
    }
}

impl<K: BkeyV<V: PtrsBkey>> BkeyVPtrs for K {}

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

// Packing, bch2_extent_crc_pack(): each entry type's pack() is None unless
// @crc fits it exactly, where C truncates.

/// A size as a crc entry has it, biased by 1.
fn crc_size_raw<T: TryFrom<u32>>(size: u32) -> Option<T> {
    T::try_from(size.checked_sub(1)?).ok()
}

/// @crc's checksum type's size in bytes, if it's one this version knows.
fn crc_csum_bytes(crc: &c::bch_extent_crc_unpacked) -> Option<u32> {
    c::bch_csum_type(u32::from(crc.csum_type)).bytes()
}

/// The checksum's bytes, lo's then hi's - where a packed entry's come from.
fn csum_bytes(csum: &c::bch_csum) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&csum.lo.get().to_le_bytes());
    b[8..].copy_from_slice(&csum.hi.get().to_le_bytes());
    b
}

impl c::bch_extent_crc32 {
    /// @crc as a crc32 entry, if it fits: a checksum of at most 4 bytes, and
    /// no nonce.
    pub fn pack(crc: &c::bch_extent_crc_unpacked) -> Option<Self> {
        if crc_csum_bytes(crc)? > 4 || u32::from(crc.nonce) > c::CRC32_NONCE_MAX {
            return None;
        }
        let csum = csum_bytes(&crc.csum);

        Self::new()
            .with_compressed_size_raw_checked(crc_size_raw(crc.compressed_size)?).ok()?
            .with_uncompressed_size_raw_checked(crc_size_raw(crc.uncompressed_size)?).ok()?
            .with_offset_checked(crc.offset.try_into().ok()?).ok()?
            .with_csum_type_checked(crc.csum_type).ok()?
            .with_compression_type_checked(crc.compression_type).ok()?
            .with_csum_checked(u32::from_ne_bytes([csum[0], csum[1], csum[2], csum[3]])).ok()
    }
}

impl c::bch_extent_crc64 {
    /// @crc as a crc64 entry, if it fits: a checksum of at most 10 bytes.
    pub fn pack(crc: &c::bch_extent_crc_unpacked) -> Option<Self> {
        if crc_csum_bytes(crc)? > 10 {
            return None;
        }
        let csum = csum_bytes(&crc.csum);

        Some(c::bch_extent_crc64 {
            bits: c::bch_extent_crc64_bits::new()
                .with_compressed_size_raw_checked(crc_size_raw(crc.compressed_size)?).ok()?
                .with_uncompressed_size_raw_checked(crc_size_raw(crc.uncompressed_size)?).ok()?
                .with_offset_checked(crc.offset).ok()?
                .with_nonce_checked(crc.nonce).ok()?
                .with_csum_type_checked(crc.csum_type).ok()?
                .with_compression_type_checked(crc.compression_type).ok()?
                .with_csum_hi_checked(u16::from_ne_bytes([csum[8], csum[9]])).ok()?,
            csum_lo: u64::from_ne_bytes(csum[..8].try_into().unwrap()),
        })
    }
}

impl c::bch_extent_crc128 {
    /// @crc as a crc128 entry, if it fits - a checksum type this version
    /// knows, any of which this holds.
    pub fn pack(crc: &c::bch_extent_crc_unpacked) -> Option<Self> {
        crc_csum_bytes(crc)?;

        Some(c::bch_extent_crc128 {
            bits: c::bch_extent_crc128_bits::new()
                .with_compressed_size_raw_checked(crc_size_raw(crc.compressed_size)?).ok()?
                .with_uncompressed_size_raw_checked(crc_size_raw(crc.uncompressed_size)?).ok()?
                .with_offset_checked(crc.offset).ok()?
                .with_nonce_checked(crc.nonce).ok()?
                .with_csum_type_checked(crc.csum_type).ok()?
                .with_compression_type_checked(crc.compression_type).ok()?,
            csum: crc.csum,
        })
    }
}

/// Whether @crc's data is compressed - incompressible isn't: crc_is_compressed().
pub fn crc_is_compressed(crc: &c::bch_extent_crc_unpacked) -> bool {
    crc.compression_type as u32 != c::bch_compression_type::BCH_COMPRESSION_TYPE_none.0 &&
        crc.compression_type as u32 != c::bch_compression_type::BCH_COMPRESSION_TYPE_incompressible.0
}

/// Whether @crc's data is checksummed or compressed - read back whole, so
/// bounded by encoded_extent_max.
pub fn crc_is_encoded(crc: &c::bch_extent_crc_unpacked) -> bool {
    crc.csum_type as u32 != c::bch_csum_type::BCH_CSUM_none.0 || crc_is_compressed(crc)
}

impl BkeySC<'_> {
    /// For reserving space to rewrite it: tolerates bad pointers.
    pub fn durability_safe(self, fs: &Fs) -> c::bkey_durability {
        unsafe { c::bch2_bkey_durability_safe(fs.as_ptr(), self.into()) }
    }
}

/// Drop @k's stale cached pointers, updating it through @iter.
pub fn drop_stale_ptrs<'a, 't>(
    t:    &TransAttempt<'a, 't>,
    iter: &BtreeIter<'t>,
    k:    BkeySC<'_>,
) -> Result<(), BchError> {
    let ret = unsafe { c::bch2_bkey_drop_stale_ptrs(t.raw(), iter.raw(), k.into()) };
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

    /// walk()'s types for an Unknown, Invalid or PastEnd entry.
    const UNKNOWN: u32 = u32::MAX;
    const INVALID: u32 = u32::MAX - 1;
    const PAST_END: u32 = u32::MAX - 2;

    /// The type of each entry in @words, and where it is, in u64s: walked by
    /// @table, as a filesystem's.
    fn walk(table: &[u8], mut words: std::vec::Vec<u64>) -> std::vec::Vec<(u32, usize)> {
        let mut sizes = EntrySizes { u64s: [0; 31], nr: table.len() as u8 };
        sizes.u64s[..table.len()].copy_from_slice(table);
        let start = words.as_ptr() as usize;
        let at = |p: *const u8| (p as usize - start) / 8;
        Entries { sizes, b: bytes(&mut words) }
            .map(|(ty, b, stop)| ExtentEntry::new(ty, b, stop))
            .map(|e| match e {
                ExtentEntry::ptr(a)        => (0, at(a as *const _ as _)),
                ExtentEntry::crc32(a)      => (1, at(a as *const _ as _)),
                ExtentEntry::crc64(a)      => (2, at(a as *const _ as _)),
                ExtentEntry::crc128(a)     => (3, at(a as *const _ as _)),
                ExtentEntry::stripe_ptr(a) => (4, at(a as *const _ as _)),
                ExtentEntry::Unknown { bytes, .. } => (UNKNOWN, at(bytes.as_ptr())),
                ExtentEntry::Invalid { bytes, .. } => (INVALID, at(bytes.as_ptr())),
                ExtentEntry::PastEnd { bytes, .. } => (PAST_END, at(bytes.as_ptr())),
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

        // where it can't step, the rest of the value: a type the filesystem
        // doesn't know, a type word of zero - and a crc128 two u64s long
        assert_eq!(walk(&ours(), vec![word(0, 1), word(20, 0), word(0, 1)]), [(0, 0), (INVALID, 1)]);
        assert_eq!(walk(&ours(), vec![word(0, 1), 0, word(0, 1)]), [(0, 0), (INVALID, 1)]);
        assert_eq!(walk(&ours(), vec![word(0, 1), word(3, 0), 0]), [(0, 0), (PAST_END, 1)]);

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
        assert!(matches!(ExtentEntryMut::new(4, bytes(&mut w), None), ExtentEntryMut::stripe_ptr(_)));
        assert!(matches!(ExtentEntryMut::new(3, bytes(&mut w), None), ExtentEntryMut::Unknown { ty: 3, .. }),
                "a crc128 doesn't fit in a word");
    }
}
