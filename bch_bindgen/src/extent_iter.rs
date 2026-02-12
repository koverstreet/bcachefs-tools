use crate::c;
use std::marker::PhantomData;

// Pull in generated extent_entry_type_u64s() from build.rs
include!(concat!(env!("OUT_DIR"), "/extent_entry_types_gen.rs"));

/// Get extent entry type from bit-position encoding (__ffs equivalent).
///
/// Returns `u32::MAX` if the type field is zero (invalid).
pub fn extent_entry_type(entry: &c::bch_extent_entry) -> u32 {
    let t = unsafe { entry.type_ } as u64;
    if t != 0 { t.trailing_zeros() } else { u32::MAX }
}

/// sizeof(struct bkey) / sizeof(u64)
const BKEY_U64S: usize = 5;

/// Get the start and end pointers for extent entries within a bkey_i.
///
/// Returns `None` for key types that don't contain extent entries.
fn bkey_ptrs_raw(k: &c::bkey_i) -> Option<(*const c::bch_extent_entry, *const c::bch_extent_entry)> {
    let val_ptr = &k.v as *const c::bch_val as *const u8;
    let val_u64s = k.k.u64s as usize - BKEY_U64S;
    let val_end = unsafe { val_ptr.add(val_u64s * 8) } as *const c::bch_extent_entry;

    use c::bch_bkey_type::*;
    use std::mem::size_of;

    let ty = k.k.type_ as u32;
    match ty {
        x if x == KEY_TYPE_btree_ptr as u32 ||
             x == KEY_TYPE_extent as u32 =>
            Some((val_ptr as *const c::bch_extent_entry, val_end)),
        x if x == KEY_TYPE_stripe as u32 => {
            let s = val_ptr as *const c::bch_stripe;
            let nr_blocks = unsafe { (*s).nr_blocks } as usize;
            let hdr = size_of::<c::bch_stripe>();
            let ptrs_start = unsafe { val_ptr.add(hdr) } as *const c::bch_extent_entry;
            let ptrs_end = unsafe { val_ptr.add(hdr + nr_blocks * 8) } as *const c::bch_extent_entry;
            Some((ptrs_start, ptrs_end))
        }
        x if x == KEY_TYPE_reflink_v as u32 => {
            let hdr = size_of::<c::bch_reflink_v>();
            let start = unsafe { val_ptr.add(hdr) } as *const c::bch_extent_entry;
            Some((start, val_end))
        }
        x if x == KEY_TYPE_btree_ptr_v2 as u32 => {
            let hdr = size_of::<c::bch_btree_ptr_v2>();
            let start = unsafe { val_ptr.add(hdr) } as *const c::bch_extent_entry;
            Some((start, val_end))
        }
        _ => None,
    }
}

/// Iterator over extent entries within a bkey.
pub struct ExtentEntryIter<'a> {
    cur: *const c::bch_extent_entry,
    end: *const c::bch_extent_entry,
    _phantom: PhantomData<&'a c::bkey_i>,
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
pub fn bkey_extent_entries(k: &c::bkey_i) -> ExtentEntryIter<'_> {
    match bkey_ptrs_raw(k) {
        Some((start, end)) => ExtentEntryIter { cur: start, end, _phantom: PhantomData },
        None => ExtentEntryIter {
            cur: std::ptr::null(),
            end: std::ptr::null(),
            _phantom: PhantomData,
        },
    }
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

/// Iterate over extent pointers in a bkey, skipping non-pointer entries.
///
/// Returns an empty iterator for key types that don't have extent pointers.
pub fn bkey_ptrs(k: &c::bkey_i) -> ExtentPtrIter<'_> {
    ExtentPtrIter { inner: bkey_extent_entries(k) }
}
