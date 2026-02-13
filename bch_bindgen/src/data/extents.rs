use crate::bkey::BkeyValSC;
use crate::c;
use std::marker::PhantomData;
use std::mem::size_of;

// Pull in generated extent_entry_type_u64s() from build.rs
include!(concat!(env!("OUT_DIR"), "/extent_entry_types_gen.rs"));

/// Get extent entry type from bit-position encoding (__ffs equivalent).
///
/// Returns `u32::MAX` if the type field is zero (invalid).
pub fn extent_entry_type(entry: &c::bch_extent_entry) -> u32 {
    let t = unsafe { entry.type_ };
    if t != 0 { t.trailing_zeros() } else { u32::MAX }
}

/// Pointer past the last val u64 for a bkey.
///
/// # Safety
/// `v` must point to the start of the value region for `k`.
unsafe fn bkey_val_end(k: &c::bkey, v: *const u8) -> *const c::bch_extent_entry {
    let val_u64s = k.u64s as usize - size_of::<c::bkey>() / 8;
    v.add(val_u64s * 8) as *const c::bch_extent_entry
}

/// Get the start and end pointers for extent entries from a typed bkey.
fn bkey_ptrs_raw(sc: &BkeyValSC<'_>) -> Option<(*const c::bch_extent_entry, *const c::bch_extent_entry)> {
    // Safety: all typed value pointers come from BkeyValSC dispatch,
    // which guarantees they point to valid bkey value data.
    unsafe { match sc {
        BkeyValSC::btree_ptr(k, v) =>
            Some((v.start.as_ptr() as _, bkey_val_end(k, *v as *const _ as _))),
        BkeyValSC::extent(k, v) =>
            Some((v.start.as_ptr() as _, bkey_val_end(k, *v as *const _ as _))),
        BkeyValSC::stripe(_k, v) =>
            Some((v.ptrs.as_ptr() as _, v.ptrs.as_ptr().add(v.nr_blocks as usize) as _)),
        BkeyValSC::reflink_v(k, v) =>
            Some((v.start.as_ptr() as _, bkey_val_end(k, *v as *const _ as _))),
        BkeyValSC::btree_ptr_v2(k, v) =>
            Some((v.start.as_ptr() as _, bkey_val_end(k, *v as *const _ as _))),
        _ => None,
    } }
}

fn empty_iter<'a>() -> ExtentEntryIter<'a> {
    ExtentEntryIter { cur: std::ptr::null(), end: std::ptr::null(), _phantom: PhantomData }
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

/// Iterate over all extent entries in a typed bkey.
///
/// Returns an empty iterator for key types that don't have extent entries.
pub fn bkey_extent_entries_sc<'a>(sc: &BkeyValSC<'a>) -> ExtentEntryIter<'a> {
    match bkey_ptrs_raw(sc) {
        Some((start, end)) => ExtentEntryIter { cur: start, end, _phantom: PhantomData },
        None => empty_iter(),
    }
}

/// Iterate over all extent entries in a `bkey_i`.
pub fn bkey_extent_entries(k: &c::bkey_i) -> ExtentEntryIter<'_> {
    bkey_extent_entries_sc(&BkeyValSC::from_bkey_i(k))
}

/// Iterator over extent pointers within a bkey.
pub struct ExtentPtrIter<'a> {
    inner: ExtentEntryIter<'a>,
}

impl<'a> Iterator for ExtentPtrIter<'a> {
    type Item = &'a c::bch_extent_ptr;

    fn next(&mut self) -> Option<Self::Item> {
        for entry in self.inner.by_ref() {
            if extent_entry_type(entry) == c::bch_extent_entry_type::BCH_EXTENT_ENTRY_ptr as u32 {
                return Some(unsafe { &entry.ptr });
            }
        }
        None
    }
}

/// Iterate over extent pointers in a typed bkey, skipping non-pointer entries.
pub fn bkey_ptrs_sc<'a>(sc: &BkeyValSC<'a>) -> ExtentPtrIter<'a> {
    ExtentPtrIter { inner: bkey_extent_entries_sc(sc) }
}

/// Iterate over extent pointers in a `bkey_i`.
pub fn bkey_ptrs(k: &c::bkey_i) -> ExtentPtrIter<'_> {
    bkey_ptrs_sc(&BkeyValSC::from_bkey_i(k))
}
