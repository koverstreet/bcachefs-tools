use crate::btree::bkey::{BkeyS, BkeySC};
use crate::btree::iter::{BtreeIter, TransAttempt};
use crate::errcode::BchError;
use crate::c;
use crate::fs::Fs;
use core::marker::PhantomData;
use core::mem::size_of;

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

trait ExtentUnionField<T> {
    unsafe fn as_union_ref(&self) -> &T;
    unsafe fn as_union_mut(&mut self) -> &mut T;
}

impl<T> ExtentUnionField<T> for c::__BindgenUnionField<T> {
    unsafe fn as_union_ref(&self) -> &T {
        unsafe { self.as_ref() }
    }
    unsafe fn as_union_mut(&mut self) -> &mut T {
        unsafe { self.as_mut() }
    }
}

impl ExtentUnionField<core::ffi::c_ulong> for core::ffi::c_ulong {
    unsafe fn as_union_ref(&self) -> &core::ffi::c_ulong {
        self
    }
    unsafe fn as_union_mut(&mut self) -> &mut core::ffi::c_ulong {
        self
    }
}

impl ExtentUnionField<c::bch_extent_ptr> for c::bch_extent_ptr {
    unsafe fn as_union_ref(&self) -> &c::bch_extent_ptr {
        self
    }
    unsafe fn as_union_mut(&mut self) -> &mut c::bch_extent_ptr {
        self
    }
}

impl ExtentUnionField<c::bch_extent_stripe_ptr> for c::bch_extent_stripe_ptr {
    unsafe fn as_union_ref(&self) -> &c::bch_extent_stripe_ptr {
        self
    }
    unsafe fn as_union_mut(&mut self) -> &mut c::bch_extent_stripe_ptr {
        self
    }
}

unsafe fn extent_union_field_ref<T, F: ExtentUnionField<T>>(field: &F) -> &T {
    unsafe { field.as_union_ref() }
}

unsafe fn extent_union_field_mut<T, F: ExtentUnionField<T>>(field: &mut F) -> &mut T {
    unsafe { field.as_union_mut() }
}

/// Get extent entry type from bit-position encoding (__ffs equivalent).
///
/// Returns `u32::MAX` if the type field is zero (invalid).
pub fn extent_entry_type(entry: &c::bch_extent_entry) -> u32 {
    let t = unsafe { *extent_union_field_ref(&entry.type_) } as u64;
    if t != 0 { t.trailing_zeros() } else { u32::MAX }
}

/// Iterator over extent entries within a bkey.
pub struct ExtentEntryIter<'a> {
    cur: *const c::bch_extent_entry,
    end: *const c::bch_extent_entry,
    _phantom: PhantomData<&'a c::bch_extent_entry>,
}

impl<'a> Iterator for ExtentEntryIter<'a> {
    type Item = &'a c::bch_extent_entry;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cur >= self.end {
            return None;
        }
        let entry = unsafe { &*self.cur };
        let ty = extent_entry_type(entry);
        let u64s = extent_entry_type_u64s(ty)?;
        let next = unsafe { (self.cur as *const u64).add(u64s) as *const c::bch_extent_entry };
        if next > self.end {
            return None;
        }
        self.cur = next;
        Some(entry)
    }
}

/// Iterate over all extent entries in a bkey.
///
/// Returns an empty iterator for key types that don't have extent entries.
pub fn bkey_extent_entries_sc(k: BkeySC<'_>) -> ExtentEntryIter<'_> {
    let b = k.extent_entry_bytes().as_ptr_range();
    ExtentEntryIter { cur: b.start.cast(), end: b.end.cast(), _phantom: PhantomData }
}

/// Iterate over all extent entries in a `bkey_i`.
pub fn bkey_extent_entries(k: &c::bkey_i) -> ExtentEntryIter<'_> {
    bkey_extent_entries_sc(k.into())
}

/// Iterator over extent pointers within a bkey.
pub struct ExtentPtrIter<'a> {
    inner: ExtentEntryIter<'a>,
}

impl<'a> Iterator for ExtentPtrIter<'a> {
    type Item = &'a c::bch_extent_ptr;

    fn next(&mut self) -> Option<Self::Item> {
        for entry in self.inner.by_ref() {
            if extent_entry_type(entry) == c::bch_extent_entry_type::BCH_EXTENT_ENTRY_ptr.0 as u32 {
                return Some(unsafe { extent_union_field_ref(&entry.ptr) });
            }
        }
        None
    }
}

/// Iterate over extent pointers in a bkey, skipping non-pointer entries.
pub fn bkey_ptrs_sc(k: BkeySC<'_>) -> ExtentPtrIter<'_> {
    ExtentPtrIter { inner: bkey_extent_entries_sc(k) }
}

/// Iterate over extent pointers in a `bkey_i`.
pub fn bkey_ptrs(k: &c::bkey_i) -> ExtentPtrIter<'_> {
    bkey_ptrs_sc(k.into())
}

pub struct ExtentEntryIterMut<'a> {
    fs:       &'a Fs,
    cur:      *mut c::bch_extent_entry,
    end:      *mut c::bch_extent_entry,
    _phantom: PhantomData<&'a mut c::bch_extent_entry>,
}

impl<'a> Iterator for ExtentEntryIterMut<'a> {
    type Item = &'a mut c::bch_extent_entry;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cur >= self.end {
            return None;
        }

        let entry = unsafe { &mut *self.cur };
        let u64s = entry_u64s(self.fs, entry);
        if u64s == 0 {
            return None;
        }

        let next = unsafe { (self.cur as *mut u64).add(u64s) as *mut c::bch_extent_entry };
        if next > self.end {
            return None;
        }

        self.cur = next;
        Some(entry)
    }
}

pub(crate) fn bkey_extent_entries_mut<'a>(
    fs: &'a Fs,
    k:  &'a mut BkeyS<'_>,
) -> ExtentEntryIterMut<'a> {
    let b = k.extent_entry_bytes_mut().as_mut_ptr_range();
    ExtentEntryIterMut { fs, cur: b.start.cast(), end: b.end.cast(), _phantom: PhantomData }
}

/// @entry's size in u64s, from this filesystem's table - which can know
/// types this code doesn't: as extent_entry_u64s().
fn entry_u64s(fs: &Fs, entry: &c::bch_extent_entry) -> usize {
    let ty = extent_entry_type(entry) as usize;
    let sb = unsafe { &(*fs.raw).sb };
    assert!(ty < sb.extent_types_known as usize,
            "extent entry type {ty}, of {} known: a validated key has none", sb.extent_types_known);
    sb.extent_type_u64s[ty] as usize
}

pub struct ExtentPtrIterMut<'a> {
    inner: ExtentEntryIterMut<'a>,
}

impl<'a> Iterator for ExtentPtrIterMut<'a> {
    type Item = &'a mut c::bch_extent_ptr;

    fn next(&mut self) -> Option<Self::Item> {
        for entry in self.inner.by_ref() {
            if extent_entry_type(entry) == c::bch_extent_entry_type::BCH_EXTENT_ENTRY_ptr.0 as u32 {
                return Some(unsafe { extent_union_field_mut(&mut entry.ptr) });
            }
        }
        None
    }
}

pub fn bkey_ptrs_mut<'a>(
    fs: &'a Fs,
    k:  &'a mut BkeyS<'_>,
) -> ExtentPtrIterMut<'a> {
    ExtentPtrIterMut { inner: bkey_extent_entries_mut(fs, k) }
}

fn extent_entry_is_crc(entry: &c::bch_extent_entry) -> bool {

    let ty = extent_entry_type(entry);
    ty == c::bch_extent_entry_type::BCH_EXTENT_ENTRY_crc32.0 as u32 ||
    ty == c::bch_extent_entry_type::BCH_EXTENT_ENTRY_crc64.0 as u32 ||
    ty == c::bch_extent_entry_type::BCH_EXTENT_ENTRY_crc128.0 as u32
}

/// The checksum/compression entries of @k, unpacked: as bkey_for_each_crc().
pub fn bkey_crcs<'a>(k: BkeySC<'a>) -> impl Iterator<Item = c::bch_extent_crc_unpacked> + 'a {
    bkey_extent_entries_sc(k)
        .filter(|e| extent_entry_is_crc(e))
        .map(move |e| unsafe {
            c::bch2_extent_crc_unpack(k.k, e as *const _ as *const c::bch_extent_crc)
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

/// Mutable access to an entry's `stripe_ptr` union field.
///
/// The caller must have checked that this entry is a stripe_ptr, i.e.
/// `extent_entry_type(entry) == BCH_EXTENT_ENTRY_stripe_ptr`.
pub(crate) fn entry_stripe_ptr_mut(
    entry: &mut c::bch_extent_entry,
) -> &mut c::bch_extent_stripe_ptr {
    unsafe { extent_union_field_mut(&mut entry.stripe_ptr) }
}
