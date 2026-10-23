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
//!
//! The bkey methods - validate, to_text, swab - are ptrs_ops.rs's; editing a
//! key's entries, ptrs_edit.rs's; picking a pointer to read, ptrs_read.rs's;
//! C's names for all of it, ptrs_c_export.rs's.

use crate::alloc::disk_groups::Target;
use crate::btree::bkey::{bkey_family, BkeyS, BkeySC, BkeyV, BkeyVal, BkeyVMut};
use crate::btree::bkey_methods::SetError;
use crate::btree::iter::{BtreeIter, TransAttempt, UpdateTriggerFlags};
use crate::data::checksum::BCH_NONCE_EXTENT;
use crate::errcode::BchError;
use crate::c;
use crate::fs::Fs;
use crate::sb::counters::events;
use crate::typeinfo::{AccessError, ResolveError};
use crate::types::{Le32, Le64};
use crate::util::rcu::{read_lock, ReadGuard};
use crate::util::vstructs::{cast_plain, cast_plain_mut, plain_bytes, ArmRef, Validated};
use arrayvec::ArrayVec;
use core::mem::{offset_of, size_of};
use core::ops::Range;

#[path = "ptrs_edit.rs"]
mod edit;
#[path = "ptrs_read.rs"]
mod read;
#[path = "ptrs_ops.rs"]
mod ops;
#[path = "ptrs_c_export.rs"]
pub mod c_export;

pub use edit::BkeyVPtrsMut;
pub use ops::{bkey_ptrs_to_text, bkey_ptrs_validate, btree_ptr_v2_compat, btree_ptr_v2_to_text, ptr_swab};
pub(crate) use ops::extent_ptr_to_text;

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
            /// Its bytes, in the key.
            pub fn bytes(&self) -> &'a [u8] {
                match *self {
                    $(Self::$name(e) => plain_bytes(e),)*
                    Self::Unknown { bytes, .. } | Self::Invalid { bytes, .. } |
                    Self::PastEnd { bytes, .. } => bytes,
                }
            }

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

        impl ExtentEntryMut<'_> {
            /// The entry, to read.
            pub fn as_entry(&self) -> ExtentEntry<'_> {
                match self {
                    $(Self::$name(e) => ExtentEntry::$name(e),)*
                    Self::Unknown { ty, bytes } => ExtentEntry::Unknown { ty: *ty, bytes },
                    Self::Invalid { ty, bytes } => ExtentEntry::Invalid { ty: *ty, bytes },
                    Self::PastEnd { ty, bytes } => ExtentEntry::PastEnd { ty: *ty, bytes },
                }
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
        EntrySizes { u64s: fs.sb.extent_type_u64s(), nr: fs.sb.extent_types_known.get() }
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

/// A value with pointers: where its extent entries are, and how one of its
/// pointers is dropped.
pub trait PtrsBkey: BkeyVal<State = Validated> {
    fn entry_range(k: BkeySC<'_, Self>) -> Range<usize>;

    /// Drop the pointer at @at, and what was only its; a type whose entries
    /// can't be dropped overrides it.
    fn drop_ptr_noerror(k: BkeyS<'_, Self>, fs: &Fs, at: EntryPos) {
        k.untyped().arm::<PtrsMut>().expect("dropping a pointer of a key whose entries can't be")
            .drop_ptr_entries(fs, at);
    }
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

    /// A stripe's pointers are its blocks, which can't be dropped: one is made
    /// to point nowhere, its dev BCH_SB_MEMBER_INVALID - for fsck's repairs.
    fn drop_ptr_noerror(mut k: BkeyS<'_, Self>, fs: &Fs, at: EntryPos) {
        k.ptr_mut_at(fs, at).expect("a pointer at the position to drop")
            .set_dev(c::BCH_SB_MEMBER_INVALID as u8);
    }
}

bkey_family!(Ptrs);
bkey_family!(PtrsMut);

/// A family of key types with pointers is a PtrsBkey as its member is.
macro_rules! ptrs_bkey {
    ($family:ident, $typed:ident; $(($name:ident, $nr:literal, $($rest:tt)*)),* $(,)?) => {
        impl PtrsBkey for $family {
            fn entry_range(k: BkeySC<'_, $family>) -> Range<usize> {
                match k.typed() {
                    $($typed::$name(k) => PtrsBkey::entry_range(k),)*
                }
            }

            fn drop_ptr_noerror(k: BkeyS<'_, $family>, fs: &Fs, at: EntryPos) {
                match k.typed() {
                    $($typed::$name(k) => PtrsBkey::drop_ptr_noerror(k, fs, at),)*
                }
            }
        }
    };
}
c::BCH_BKEY_TYPES!(cstruct_macros::xmacro_select [BKEY_TYPE_ptrs => ptrs_bkey { Ptrs, PtrsTyped; }]);
c::BCH_BKEY_TYPES!(cstruct_macros::xmacro_select [BKEY_TYPE_ptrs_mut => ptrs_bkey { PtrsMut, PtrsMutTyped; }]);

macro_rules! key_types_mask {
    ($mask:ident; $(($name:ident, $nr:literal, $($rest:tt)*)),* $(,)?) => {
        const $mask: u64 = 0 $(| 1 << $nr)*;
    };
}
c::BCH_BKEY_TYPES!(cstruct_macros::xmacro_select [BKEY_TYPE_ptrs => key_types_mask { PTRS_KEY_TYPES; }]);
c::BCH_BKEY_TYPES!(cstruct_macros::xmacro_select [BKEY_TYPE_ptrs_mut => key_types_mask { PTRS_MUT_KEY_TYPES; }]);

const _: () = assert!(PTRS_MUT_KEY_TYPES & !PTRS_KEY_TYPES == 0,
                      "a BKEY_TYPE_ptrs_mut type that isn't BKEY_TYPE_ptrs");

/// The extent entries of a key with pointers, in whichever form it's held:
/// any BkeyV whose value is a PtrsBkey - armed to Ptrs, or to one of them.
pub trait BkeyVPtrs: BkeyV<V: PtrsBkey> {
    /// Its extent entries' bytes, what bkey_ptrs_c() spans: empty for a value
    /// too short to hold them.
    fn entry_bytes(&self) -> &[u8] {
        let k = self.as_sc();
        k.val_bytes().get(<Self::V as PtrsBkey>::entry_range(k)).unwrap_or_default()
    }

    fn extent_entries(&self, fs: &Fs) -> impl Iterator<Item = ExtentEntry<'_>> {
        Entries { sizes: EntrySizes::of(fs), b: self.entry_bytes() }.map(|(ty, b, stop)| ExtentEntry::new(ty, b, stop))
    }

    /// extent_entries(), each with where it is.
    fn extent_entries_at(&self, fs: &Fs) -> impl Iterator<Item = (EntryPos, ExtentEntry<'_>)> {
        let val = self.as_sc().val_bytes().as_ptr() as usize;
        self.extent_entries(fs).map(move |e| (EntryPos((e.bytes().as_ptr() as usize - val) / 8), e))
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

    /// Each pointer with the last crc before it and the stripe_ptr just before
    /// it: bkey_for_each_ptr_decode(). Where the walk stops, it ends, as
    /// extent_entry_next_safe().
    fn ptrs_decoded(&self, fs: &Fs) -> impl Iterator<Item = DecodedPtr<'_>> {
        let k = self.k();
        // bch2_extent_crc_unpack(k, NULL)
        let mut crc = c::bch_extent_crc_unpacked {
            compressed_size:   k.size,
            uncompressed_size: k.size,
            live_size:         k.size,
            ..Default::default()
        };
        let mut ec = None;

        self.extent_entries_at(fs).filter_map(move |(pos, e)| match e {
            ExtentEntry::ptr(ptr) => Some(DecodedPtr {
                p: c::extent_ptr_decoded {
                    has_ec: ec.is_some(),
                    crc,
                    ptr: *ptr,
                    ec: ec.take().unwrap_or_default(),
                    ..Default::default()
                },
                ptr,
                pos,
            }),
            ExtentEntry::stripe_ptr(s) => {
                ec = Some(*s);
                None
            }
            e => {
                if let Some(c) = crc_unpack(k, e) {
                    crc = c;
                }
                None
            }
        })
    }

    /// Whether it has data that didn't compress - a crc of compression type
    /// incompressible: bch2_bkey_is_incompressible().
    fn is_incompressible(&self, fs: &Fs) -> bool {
        let incompressible = c::bch_compression_type::BCH_COMPRESSION_TYPE_incompressible.0 as u8;
        self.crcs(fs).any(|crc| crc.compression_type == incompressible)
    }

    /// If @src has data that didn't compress, so does this, copied from it:
    /// its uncompressed crcs made incompressible, in place -
    /// bch2_bkey_propagate_incompressible(). @src of any type; one without
    /// pointers has no such data.
    ///
    /// XXX (C's): if some data actually is compressed, we want
    /// bch_extent_reconcile.wont_recompress_smaller
    fn propagate_incompressible(&mut self, fs: &Fs, src: BkeySC<'_>)
    where Self: BkeyVMut
    {
        if !src.arm::<Ptrs>().is_some_and(|s| s.is_incompressible(fs)) {
            return;
        }

        use c::bch_compression_type as t;
        let (none, incompressible) = (t::BCH_COMPRESSION_TYPE_none.0 as u8,
                                      t::BCH_COMPRESSION_TYPE_incompressible.0 as u8);
        for e in self.extent_entries_mut(fs) {
            match e {
                ExtentEntryMut::crc32(e) if e.compression_type() == none => e.set_compression_type(incompressible),
                ExtentEntryMut::crc64(e) if e.compression_type() == none => e.set_compression_type(incompressible),
                ExtentEntryMut::crc128(e) if e.compression_type() == none => e.set_compression_type(incompressible),
                _ => {}
            }
        }
    }

    /// Its extent flags - its flags entry's, which comes first, if it has one:
    /// bch2_bkey_extent_flags().
    fn extent_flags(&self, fs: &Fs) -> u64 {
        match self.extent_entries(fs).next() {
            Some(ExtentEntry::flags(f)) => f.flags(),
            _ => 0,
        }
    }

    /// Its pointer to device @dev, if it has one: bch2_bkey_has_device_c().
    fn has_device(&self, fs: &Fs, dev: u32) -> Option<&c::bch_extent_ptr> {
        self.ptrs(fs).find(|p| u32::from(p.dev()) == dev)
    }

    /// Its pointer to device @dev, decoded: bch2_bkey_has_device_decode().
    fn has_device_decode(&self, fs: &Fs, dev: u32) -> Option<DecodedPtr<'_>> {
        self.ptrs_decoded(fs).find(|d| u32::from(d.ptr.dev()) == dev)
    }

    /// Whether the allocator writes to every device it points to:
    /// bch2_bkey_devs_rw().
    fn devs_rw(&self, fs: &Fs) -> bool {
        let rcu = read_lock();
        self.ptrs(fs).all(|p| fs.dev_rcu(p.dev().into(), &rcu).is_some_and(|ca| ca.is_rw()))
    }

    /// Whether it has its data in target @target - a pointer there that isn't
    /// cached, or is and isn't stale: bch2_bkey_has_target().
    fn has_target(&self, fs: &Fs, target: Target) -> bool {
        let rcu = read_lock();
        self.ptrs(fs).any(|p| {
            fs.dev_in_target(p.dev().into(), target, &rcu) &&
                fs.dev_rcu(p.dev().into(), &rcu)
                    .is_some_and(|ca| p.cached() == 0 || ca.ptr_stale(p, &rcu) == Some(0))
        })
    }

    /// Whether every pointer is in target @target: bch2_bkey_in_target().
    fn in_target(&self, fs: &Fs, target: Target) -> bool {
        let rcu = read_lock();
        self.ptrs(fs).all(|p| fs.dev_in_target(p.dev().into(), target, &rcu))
    }

    /// Whether it has pointer @m, to data @offset sectors into its extent -
    /// the device, gen and place on disk, given where each extent starts:
    /// bch2_bkey_matches_ptr().
    fn matches_ptr(&self, fs: &Fs, m: &c::bch_extent_ptr, offset: u64) -> bool {
        let start = self.k().start_offset();
        self.ptrs_decoded(fs).any(|d| {
            d.p.ptr.dev() == m.dev() &&
                d.p.ptr.generation() == m.generation() &&
                ptr_data_start(&d.p, start) == m.offset().wrapping_sub(offset)
        })
    }

    fn extent_entries_mut(&mut self, fs: &Fs) -> impl Iterator<Item = ExtentEntryMut<'_>>
    where Self: BkeyVMut
    {
        self.extent_entries_mut_at(fs).map(|(_, e)| e)
    }

    /// extent_entries_mut(), each with where it is.
    fn extent_entries_mut_at(&mut self, fs: &Fs) -> impl Iterator<Item = (EntryPos, ExtentEntryMut<'_>)>
    where Self: BkeyVMut
    {
        let val = self.as_sc().val_bytes().as_ptr() as usize;
        let range = <Self::V as PtrsBkey>::entry_range(self.as_sc());
        let b = self.as_s().into_val_bytes_mut().get_mut(range).unwrap_or_default();
        EntriesMut { sizes: EntrySizes::of(fs), b }.map(move |(ty, b, stop)| {
            (EntryPos((b.as_ptr() as usize - val) / 8), ExtentEntryMut::new(ty, b, stop))
        })
    }

    fn ptrs_mut(&mut self, fs: &Fs) -> impl Iterator<Item = &mut c::bch_extent_ptr>
    where Self: BkeyVMut
    {
        self.extent_entries_mut(fs).filter_map(|e| match e {
            ExtentEntryMut::ptr(p) => Some(p),
            _ => None,
        })
    }

    /// Its pointer at @at, to edit.
    fn ptr_mut_at(&mut self, fs: &Fs, at: EntryPos) -> Option<&mut c::bch_extent_ptr>
    where Self: BkeyVMut
    {
        self.extent_entries_mut_at(fs).find_map(|(pos, e)| match e {
            ExtentEntryMut::ptr(p) if pos == at => Some(p),
            _ => None,
        })
    }

    /// Drop the pointer at @at, as its type does - a stripe's can't be, and is
    /// made to point nowhere: bch2_bkey_drop_ptr_noerror().
    fn drop_ptr_noerror(&mut self, fs: &Fs, at: EntryPos)
    where Self: BkeyVMut
    {
        <Self::V as PtrsBkey>::drop_ptr_noerror(self.as_s(), fs, at);
    }

    /// drop_ptr_noerror() - unless a pointer to its device has a stripe,
    /// whose redundancy it keeps: then it's made to point nowhere, its dev
    /// BCH_SB_MEMBER_INVALID. bch2_bkey_drop_ptr().
    fn drop_ptr(&mut self, fs: &Fs, at: EntryPos)
    where Self: BkeyVMut
    {
        let dev = self.ptrs_decoded(fs).find(|p| p.pos == at)
            .expect("a pointer at the position to drop").p.ptr.dev();
        if self.ptrs_decoded(fs).any(|p| p.p.ptr.dev() == dev && p.p.has_ec) {
            self.ptr_mut_at(fs, at).expect("a pointer at the position to drop")
                .set_dev(c::BCH_SB_MEMBER_INVALID as u8);
        } else {
            self.drop_ptr_noerror(fs, at);
        }
    }

    /// drop_ptr_noerror() each pointer @cond picks, the walk restarted after
    /// each: bch2_bkey_drop_ptrs_noerror().
    fn drop_ptrs_noerror(&mut self, fs: &Fs, mut cond: impl FnMut(&DecodedPtr<'_>) -> bool)
    where Self: BkeyVMut
    {
        while let Some(at) = { let at = self.ptrs_decoded(fs).find(|p| cond(p)).map(|p| p.pos); at } {
            self.drop_ptr_noerror(fs, at);
        }
    }

    /// drop_ptrs_noerror(), with drop_ptr(): bch2_bkey_drop_ptrs().
    fn drop_ptrs(&mut self, fs: &Fs, mut cond: impl FnMut(&DecodedPtr<'_>) -> bool)
    where Self: BkeyVMut
    {
        while let Some(at) = { let at = self.ptrs_decoded(fs).find(|p| cond(p)).map(|p| p.pos); at } {
            self.drop_ptr(fs, at);
        }
    }

    /// Its pointers to @dev: bch2_bkey_drop_device_noerror().
    fn drop_device_noerror(&mut self, fs: &Fs, dev: u32)
    where Self: BkeyVMut
    {
        self.drop_ptrs_noerror(fs, |p| u32::from(p.p.ptr.dev()) == dev);
    }

    /// Its pointers to @dev, with drop_ptr(): bch2_bkey_drop_device().
    fn drop_device(&mut self, fs: &Fs, dev: u32)
    where Self: BkeyVMut
    {
        self.drop_ptrs(fs, |p| u32::from(p.p.ptr.dev()) == dev);
    }

    /// The pointers of @mask, by index among the pointers - the highest first,
    /// so the lower are where they were: bch2_bkey_drop_ptrs_mask().
    fn drop_ptrs_mask(&mut self, fs: &Fs, mut mask: u32)
    where Self: BkeyVMut
    {
        while mask != 0 {
            let drop = 31 - mask.leading_zeros();
            let at = self.ptrs_decoded(fs).nth(drop as usize).map(|p| p.pos);
            if let Some(at) = at {
                self.drop_ptr_noerror(fs, at);
            }
            mask ^= 1 << drop;
        }
    }

    /// Drop its cached pointer at @at, with a cached_ptr_drop event saying
    /// @why: drop_cached_pointer_trace(). Not in a read-side section, as C's
    /// is: the trace's printbuf allocates.
    fn drop_cached_ptr(&mut self, fs: &Fs, at: EntryPos, why: &str)
    where Self: BkeyVMut
    {
        let k = self.as_sc();
        let ptr = *self.ptrs_decoded(fs).find(|d| d.pos == at)
            .expect("a cached pointer at the position to drop").ptr;

        fs.event_add_trace(events::cached_ptr_drop, k.k.size.into(), |out| {
            write!(out, "{why}\ndropping pointer\n  ");
            extent_ptr_to_text(&mut out.atomic(), Some(fs), &ptr, &read_lock());
            crate::prt!(out, "\nfrom:\n{}", k.to_text(fs));
        });

        self.drop_ptr_noerror(fs, at);
    }

    /// The first of its cached pointers that shouldn't be, and why.
    fn cached_ptr_to_drop(&self, fs: &Fs, opts: &c::bch_inode_opts) -> Option<(EntryPos, &'static str)> {
        let rcu = read_lock();
        let mut have_cached = false;

        for d in self.ptrs_decoded(fs) {
            if let Some(why) = cached_ptr_drop_reason(fs, opts, &d, have_cached, &rcu) {
                return Some((d.pos, why));
            }
            have_cached |= d.ptr.cached() != 0;
        }
        None
    }

    /// Its cached pointers that shouldn't be, dropped:
    /// bch2_bkey_drop_extra_cached_ptrs().
    fn drop_extra_cached_ptrs(&mut self, fs: &Fs, opts: &c::bch_inode_opts)
    where Self: BkeyVMut
    {
        while let Some((at, why)) = self.cached_ptr_to_drop(fs, opts) {
            self.drop_cached_ptr(fs, at, why);
        }
    }

    /// Its pointer to @dev made cached, after its cached pointers that
    /// shouldn't be are dropped - and dropped itself if it shouldn't be, or
    /// if the first walk finds it erasure coded: bch2_extent_ptr_set_cached().
    ///
    /// XXX: C's - if a walk dropped the pointer itself, C dereferences NULL
    /// where this panics; only cached pointers are dropped, and the callers
    /// pass pointers that aren't.
    fn set_cached(&mut self, fs: &Fs, opts: &c::bch_inode_opts, dev: u32)
    where Self: BkeyVMut
    {
        // C's pointer to it, forgotten once a drop moves the entries
        let mut ptr_known = true;

        let have_cached = loop {
            let rcu = read_lock();
            let mut have_cached = false;
            let mut to_drop = None;

            for d in self.ptrs_decoded(fs) {
                if ptr_known && u32::from(d.ptr.dev()) == dev && d.p.has_ec {
                    to_drop = Some((d.pos, "ptr->cached && ec currently incompatible", true));
                    break;
                }
                if let Some(why) = cached_ptr_drop_reason(fs, opts, &d, have_cached, &rcu) {
                    to_drop = Some((d.pos, why, false));
                    break;
                }
                have_cached |= d.ptr.cached() != 0;
            }
            drop(rcu);

            match to_drop {
                Some((at, why, done)) => {
                    self.drop_cached_ptr(fs, at, why);
                    if done {
                        return;
                    }
                    ptr_known = false;
                }
                None => break have_cached,
            }
        };

        let at = self.ptrs_decoded(fs).filter(|d| u32::from(d.ptr.dev()) == dev).last()
            .unwrap_or_else(|| panic!("making the pointer to dev {dev} cached, which a walk dropped"))
            .pos;
        self.ptr_mut_at(fs, at).expect("the pointer just found").set_cached(1);

        let why = {
            let rcu = read_lock();
            let d = self.ptrs_decoded(fs).find(|d| d.pos == at).expect("the pointer just found");
            cached_ptr_drop_reason(fs, opts, &d, have_cached, &rcu)
        };
        if let Some(why) = why {
            self.drop_cached_ptr(fs, at, why);
        }
    }

    /// set_cached() each pointer of @mask, by index among the pointers, the
    /// highest first: bch2_bkey_set_ptrs_cached_mask(). The mask is read as
    /// devices before any is made cached - set_cached() can drop cached
    /// pointers anywhere in the key, moving the ones after them.
    fn set_ptrs_cached_mask(&mut self, fs: &Fs, opts: &c::bch_inode_opts, mask: u32)
    where Self: BkeyVMut
    {
        let devs: ArrayVec<u32, { c::BCH_BKEY_PTRS_MAX as usize }> = self.ptrs(fs).enumerate()
            .filter(|&(i, _)| i < 32 && mask & (1 << i) != 0)
            .map(|(_, p)| u32::from(p.dev()))
            .collect();

        for dev in devs.into_iter().rev() {
            self.set_cached(fs, opts, dev);
        }
    }

    /// Whether it has a cached pointer that's stale: bch2_bkey_has_stale_ptrs().
    fn has_stale_ptrs(&self, fs: &Fs) -> bool {
        let rcu = read_lock();
        self.ptrs(fs).any(|p| {
            p.cached() != 0 &&
                fs.dev_rcu(p.dev().into(), &rcu)
                    .is_some_and(|ca| ca.ptr_stale(p, &rcu).is_some_and(|stale| stale > 0))
        })
    }

    /// Its stale cached pointers dropped - and its cached pointers to devices
    /// that aren't there, which has_stale_ptrs() doesn't count:
    /// __bch2_bkey_drop_stale_ptrs().
    fn drop_stale_ptrs(&mut self, fs: &Fs)
    where Self: BkeyVMut
    {
        let rcu = read_lock();
        self.drop_ptrs_noerror(fs, |d| {
            d.ptr.cached() != 0 &&
                fs.dev_rcu(d.ptr.dev().into(), &rcu)
                    .is_none_or(|ca| ca.ptr_stale(d.ptr, &rcu).is_some_and(|stale| stale > 0))
        });
    }
}

impl<K: BkeyV<V: PtrsBkey>> BkeyVPtrs for K {}

/// Why @d shouldn't be cached, if it's a cached pointer - @have_cached if one
/// before it is: maybe_drop_cached_ptr()'s tests. A bucket its device doesn't
/// have is stale, as C's -1 is.
fn cached_ptr_drop_reason(fs: &Fs, opts: &c::bch_inode_opts, d: &DecodedPtr<'_>, have_cached: bool,
                          rcu: &ReadGuard) -> Option<&'static str> {
    let ptr = d.ptr;
    if ptr.cached() == 0 {
        return None;
    }
    if have_cached {
        return Some("extent already has another cached pointer");
    }
    if d.p.has_ec {
        return Some("cached ptr incompatible with erasure coding");
    }

    let dev = u32::from(ptr.dev());
    let ca = fs.dev_rcu(dev, rcu);
    if ca.is_some_and(|ca| ca.ptr_stale(ptr, rcu) != Some(0)) {
        return Some("pointer is stale");
    }
    if ca.is_none_or(|ca| u32::from(ca.mi.state) == c::bch_member_state::BCH_MEMBER_STATE_evacuating.0) {
        return Some("device bad or evacuating");
    }

    let target = if opts.promote_target != 0 { opts.promote_target } else { opts.foreground_target };
    if target != 0 && !fs.dev_in_target(dev, Target::decode(target.into()), rcu) {
        return Some("incorrect target");
    }
    None
}

/// A pointer, decoded: ptrs_decoded()'s. @p is C's extent_ptr_decoded, a
/// copy - its read path's fields zero; @ptr is the pointer in the key, and
/// @pos where it is, to edit it by.
#[derive(Clone, Copy)]
pub struct DecodedPtr<'a> {
    pub p: c::extent_ptr_decoded,
    pub ptr: &'a c::bch_extent_ptr,
    pub pos: EntryPos,
}

/// What @d, a pointer of @k, points to - a stripe's parity blocks are parity,
/// its others user data.
pub fn ptr_data_type(fs: &Fs, k: BkeySC<'_>, d: &DecodedPtr<'_>) -> c::bch_data_type {
    use c::bch_bkey_type as t;
    use c::bch_data_type as dt;
    match k.k.key_type() {
        t::KEY_TYPE_btree_ptr | t::KEY_TYPE_btree_ptr_v2 => dt::BCH_DATA_btree,
        t::KEY_TYPE_extent | t::KEY_TYPE_reflink_v =>
            if d.p.has_ec { dt::BCH_DATA_stripe }
            else if d.p.ptr.cached() != 0 { dt::BCH_DATA_cached }
            else { dt::BCH_DATA_user },
        t::KEY_TYPE_stripe => {
            let s = k.arm::<c::Stripe>().expect("a stripe");
            let block = s.ptrs(fs).position(|p| core::ptr::eq(p, d.ptr))
                .expect("a pointer of the stripe's");
            let s = s.v();
            assert!(block < s.nr_blocks.into(), "stripe pointer past its blocks");
            if block + usize::from(s.nr_redundant) >= s.nr_blocks.into() { dt::BCH_DATA_parity } else { dt::BCH_DATA_user }
        }
        _ => panic!("the data type of a pointer in a key without pointers"),
    }
}

/// @sectors of @p's extent, as the sectors it takes on disk: compressed, if it is.
pub fn ptr_disk_sectors(sectors: u64, p: &c::extent_ptr_decoded) -> u64 {
    if crc_is_compressed(&p.crc) {
        (sectors * u64::from(p.crc.compressed_size)).div_ceil(p.crc.uncompressed_size.into())
    } else {
        sectors
    }
}

/// Where an entry is in its key's value, in u64s from the value's start: what
/// a key's entries are edited by. Only the walk makes one, and it holds for as
/// long as nothing before it is inserted or dropped.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct EntryPos(usize);

/// @entry, if it's a crc entry of @k, unpacked: the sizes unbiased, the
/// checksum's bytes where bch_csum has them.
pub(crate) fn crc_unpack(k: &c::bkey, entry: ExtentEntry<'_>) -> Option<c::bch_extent_crc_unpacked> {
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

impl c::bch_extent_ptr {
    /// Its field @field, by C's name, set to @v: for kvdb, to make the keys
    /// fsck repairs.
    pub fn set_field<'p>(&mut self, field: &'p str, v: u64) -> Result<(), SetError<'p>> {
        let (bits, set): (u8, fn(&mut Self, u64)) = match field {
            "cached"    => (1,  |p, v| p.set_cached(v as u8)),
            "unwritten" => (1,  |p, v| p.set_unwritten(v as u8)),
            "offset"    => (44, |p, v| p.set_offset(v)),
            "dev"       => (8,  |p, v| p.set_dev(v as u8)),
            "gen"       => (8,  |p, v| p.set_generation(v as u8)),
            _ => return Err(ResolveError::NoSuchField { st: "bch_extent_ptr", field }.into()),
        };
        if v >> bits != 0 {
            return Err(SetError::Access { field, err: AccessError::BitsOverflow { bits, val: v } });
        }
        set(self, v);
        Ok(())
    }
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

impl ExtentEntryMut<'_> {
    /// Packs @crc into this crc entry, as its own type: bch2_extent_crc_pack()
    /// with the entry's type. None if it isn't a crc entry, or @crc doesn't
    /// fit it.
    pub fn set_crc(&mut self, crc: &c::bch_extent_crc_unpacked) -> Option<()> {
        match self {
            Self::crc32(e)  => **e = c::bch_extent_crc32::pack(crc)?,
            Self::crc64(e)  => **e = c::bch_extent_crc64::pack(crc)?,
            Self::crc128(e) => **e = c::bch_extent_crc128::pack(crc)?,
            _ => return None,
        }
        Some(())
    }
}

/// Whether @crc's data is compressed - incompressible isn't: crc_is_compressed().
pub fn crc_is_compressed(crc: &c::bch_extent_crc_unpacked) -> bool {
    crc.compression_type as u32 != c::bch_compression_type::BCH_COMPRESSION_TYPE_none.0 &&
        crc.compression_type as u32 != c::bch_compression_type::BCH_COMPRESSION_TYPE_incompressible.0
}

/// The nonce of @crc's data, in an extent of version @version: extent_nonce().
pub fn extent_nonce(version: c::bversion, crc: &c::bch_extent_crc_unpacked) -> c::nonce {
    let compression_type = if crc_is_compressed(crc) { u32::from(crc.compression_type) } else { 0 };
    let size = if compression_type != 0 { crc.uncompressed_size } else { 0 };
    let (lo, hi) = (version.lo, version.hi);
    let nonce = c::nonce {
        d: [
            Le32::new(size << 22),
            Le32::new(lo as u32),
            Le32::new((lo >> 32) as u32),
            // C xors the domain into the word once it's little-endian
            Le32::new((hi | compression_type << 24) ^ u32::from_le(BCH_NONCE_EXTENT)),
        ],
    };
    nonce + (u32::from(crc.nonce) << 9)
}

/// Whether @crc's data is checksummed or compressed - read back whole, so
/// bounded by encoded_extent_max.
pub fn crc_is_encoded(crc: &c::bch_extent_crc_unpacked) -> bool {
    crc.csum_type as u32 != c::bch_csum_type::BCH_CSUM_none.0 || crc_is_compressed(crc)
}

/// Drop @k's stale cached pointers, if it has any, as an update through @iter:
/// bch2_bkey_drop_stale_ptrs().
pub fn drop_stale_ptrs<'a, 't>(
    t:    &TransAttempt<'a, 't>,
    iter: &BtreeIter<'t>,
    k:    BkeySC<'_>,
) -> Result<(), BchError> {
    let fs = t.fs();
    if !k.arm::<Ptrs>().is_some_and(|p| p.has_stale_ptrs(fs)) {
        return Ok(());
    }

    let mut u = t.bkey_make_mut(iter, k, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
    BkeyS::from(&mut u).arm::<Ptrs>().expect("a key with pointers, as checked").drop_stale_ptrs(fs);
    Ok(())
}

// ── queries ──────────────────────────────────────────────────────────────

/// Where on @p's device the data of its extent, starting at @start, would
/// start: C's (s64) ptr.offset + crc.offset - bkey_start_offset(), which
/// wraps - two are compared, so wrapping or not doesn't matter.
fn ptr_data_start(p: &c::extent_ptr_decoded, start: u64) -> u64 {
    p.ptr.offset().wrapping_add(p.crc.offset.into()).wrapping_sub(start)
}

/// Whether @p1 of @k1 and @p2 of @k2 point to the same data: the same
/// device - or the same block of the same stripe - and gen, the same place
/// given where each extent starts (one may have been trimmed), and
/// overlapping on disk: bch2_bkey_ptrs_match().
pub fn ptrs_match(k1: &c::bkey, p1: &c::extent_ptr_decoded, k2: &c::bkey, p2: &c::extent_ptr_decoded) -> bool {
    let overlaps = |a: &c::extent_ptr_decoded, b: &c::extent_ptr_decoded| {
        a.ptr.offset() >= b.ptr.offset() && a.ptr.offset() < b.ptr.offset() + u64::from(b.crc.compressed_size)
    };

    (p1.ptr.dev() == p2.ptr.dev() ||
     (p1.has_ec && p2.has_ec && p1.ec.idx() == p2.ec.idx() && p1.ec.block() == p2.ec.block())) &&
        p1.ptr.generation() == p2.ptr.generation() &&
        ptr_data_start(p1, k1.start_offset()) == ptr_data_start(p2, k2.start_offset()) &&
        /*
         * The same place, given where they start, can be a coincidence when
         * one is partially overwritten by an extent written right after it
         * and compression is in use - adjacent on disk, not the same data:
         * they must overlap too.
         */
        (overlaps(p1, p2) || overlaps(p2, p1))
}

/// Whether @k1 and @k2 refer to the same data: of the same type, and for a
/// type with its data's pointers, both unwritten or neither, with a pointer
/// pair that ptrs_match(). Any other type - KEY_TYPE_deleted, say - always
/// does: bch2_extents_match().
pub fn extents_match(fs: &Fs, k1: BkeySC<'_>, k2: BkeySC<'_>) -> bool {
    if k1.k.key_type() != k2.k.key_type() {
        return false;
    }
    if !k1.k.extent_is_direct_data() {
        return true;
    }
    if k1.extent_is_unwritten(fs) != k2.extent_is_unwritten(fs) {
        return false;
    }

    let p1 = k1.arm::<Ptrs>().expect("a key with direct data has pointers");
    let p2 = k2.arm::<Ptrs>().expect("a key with direct data has pointers");
    let matched = p1.ptrs_decoded(fs).any(|a| p2.ptrs_decoded(fs).any(|b| ptrs_match(k1.k, &a.p, k2.k, &b.p)));
    matched
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

    /// @table as a filesystem's entry lengths.
    fn sizes(table: &[u8]) -> EntrySizes {
        let mut sizes = EntrySizes { u64s: [0; 31], nr: table.len() as u8 };
        sizes.u64s[..table.len()].copy_from_slice(table);
        sizes
    }

    /// walk()'s types for an Unknown, Invalid or PastEnd entry.
    const UNKNOWN: u32 = u32::MAX;
    const INVALID: u32 = u32::MAX - 1;
    const PAST_END: u32 = u32::MAX - 2;

    /// The type of each entry in @words, and where it is, in u64s: walked by
    /// @table, as a filesystem's.
    fn walk(table: &[u8], mut words: std::vec::Vec<u64>) -> std::vec::Vec<(u32, usize)> {
        let start = words.as_ptr() as usize;
        let at = |p: *const u8| (p as usize - start) / 8;
        Entries { sizes: sizes(table), b: bytes(&mut words) }
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

    fn crc(csum_type: c::bch_csum_type, size: u32, offset: u16, nonce: u16) -> c::bch_extent_crc_unpacked {
        // a checksum as wide as its type, every byte nonzero
        let n = csum_type.bytes().unwrap() as usize;
        let mut b = [0u8; 16];
        b.iter_mut().take(n).enumerate().for_each(|(i, x)| *x = 0xa0 + i as u8);
        c::bch_extent_crc_unpacked {
            compressed_size:   size,
            uncompressed_size: size,
            live_size:         size - u32::from(offset),
            csum_type:         csum_type.0 as u8,
            compression_type:  0,
            offset,
            nonce,
            csum: c::bch_csum { lo: Le64::new(u64::from_le_bytes(b[..8].try_into().unwrap())),
                                hi: Le64::new(u64::from_le_bytes(b[8..].try_into().unwrap())) },
        }
    }

    /// @v's words, as this machine has them.
    fn words_of<T: crate::util::vstructs::Plain>(v: &T) -> std::vec::Vec<u64> {
        // SAFETY: Plain, and a whole number of u64s
        let b = unsafe { core::slice::from_raw_parts((v as *const T).cast::<u8>(), size_of::<T>()) };
        b.chunks_exact(8).map(|w| u64::from_ne_bytes(w.try_into().unwrap())).collect()
    }

    #[test]
    fn ptr_swab_reads_the_other_endianness() {
        use c::bch_csum_type as t;

        let c32 = c::bch_extent_crc32::pack(&crc(t::BCH_CSUM_crc32c, 8, 0, 0)).unwrap();
        let c64 = c::bch_extent_crc64::pack(&crc(t::BCH_CSUM_chacha20_poly1305_80, 8, 0, 3)).unwrap();
        let c128 = c::bch_extent_crc128::pack(&crc(t::BCH_CSUM_chacha20_poly1305_128, 8, 0, 3)).unwrap();
        let ptr = word(0, 0x123456789);

        let native: std::vec::Vec<u64> = [words_of(&c32), vec![ptr], words_of(&c64), vec![ptr],
                                          words_of(&c128), vec![ptr]].concat();

        // As the other endianness writes it: each word's value the same, so its
        // bytes reversed - but checksums are bytes: crc128's csum and crc64's
        // csum_lo the same bytes, and the checksum bits in crc32's and crc64's
        // first word byte-swapped values.
        let mut b32 = c32;
        b32.set_csum(c32.csum().swap_bytes());
        let mut b64 = c64;
        b64.set_csum_hi(c64.csum_hi().swap_bytes());
        let c64w = words_of(&c64);
        let c128w = words_of(&c128);
        let mut other: std::vec::Vec<u64> = [
            vec![words_of(&b32)[0].swap_bytes(), ptr.swap_bytes()],
            vec![words_of(&b64)[0].swap_bytes(), c64w[1], ptr.swap_bytes()],
            vec![c128w[0].swap_bytes(), c128w[1], c128w[2], ptr.swap_bytes()],
        ].concat();

        ops::entries_swab(sizes(&ours()), bytes(&mut other));
        assert_eq!(other, native);
    }

    /// The nonces C's extent_nonce() gives on little-endian - worked by hand,
    /// as data on disk was encrypted with them. A wrong one shows only as
    /// encrypted extents whose checksums never verify.
    #[test]
    fn extent_nonce_as_c_makes_it() {
        let version = c::bversion { lo: 0x1122_3344_5566_7788, hi: 0x9a };
        let words = |n: c::nonce| n.d.map(|w| w.get());

        // not compressed: no size or compression type; crc.nonce 3 is three
        // 512 byte sectors, 24 ChaCha20 blocks
        let mut crc = crc(c::bch_csum_type::BCH_CSUM_chacha20_poly1305_128, 128, 0, 3);
        assert_eq!(words(extent_nonce(version, &crc)),
                   [24, 0x5566_7788, 0x1122_3344, 0x1000_009a]);

        // incompressible counts as not compressed
        crc.compression_type = 5;
        assert_eq!(words(extent_nonce(version, &crc)),
                   [24, 0x5566_7788, 0x1122_3344, 0x1000_009a]);

        // lz4 (3), 256 sectors uncompressed: the size at bit 22, the type at
        // bit 24 of the last word, under the extent domain at 28
        crc.compression_type = 3;
        crc.uncompressed_size = 256;
        assert_eq!(words(extent_nonce(version, &crc)),
                   [(256 << 22) + 24, 0x5566_7788, 0x1122_3344, 0x1300_009a]);
    }
}
