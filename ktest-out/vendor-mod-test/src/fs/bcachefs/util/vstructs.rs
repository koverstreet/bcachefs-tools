use crate::c;

// vstruct_next(entry) = (u64*)entry._data + le16(entry.u64s)
pub(crate) unsafe fn vstruct_next_entry(entry: *const c::jset_entry) -> *const c::jset_entry {
    let u64s = u16::from_le((*entry).u64s) as usize;
    (entry as *const u8).add(8 + u64s * 8) as *const c::jset_entry
}
