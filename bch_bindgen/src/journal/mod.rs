use crate::c;
use std::marker::PhantomData;

// ---- vstruct pointer arithmetic ----

/// jset_entry is 8 bytes; _data is at offset 0 (u64 flexible array).
/// vstruct_next(entry) = (u64*)entry._data + le16(entry.u64s)
unsafe fn vstruct_next_entry(entry: *const c::jset_entry) -> *const c::jset_entry {
    let u64s = u16::from_le((*entry).u64s) as usize;
    (entry as *const u8).add(8 + u64s * 8) as *const c::jset_entry
}

/// Pointer to one past the last entry in a jset.
unsafe fn vstruct_last_jset(jset: *const c::jset) -> *const c::jset_entry {
    let u64s = u32::from_le((*jset).u64s) as usize;
    (jset as *const u8).add(56 + u64s * 8) as *const c::jset_entry
}

/// bkey_next: advance past a bkey_i.
unsafe fn bkey_next_raw(k: *const c::bkey_i) -> *const c::bkey_i {
    let u64s = (*k).k.u64s as usize;
    (k as *const u8).add(u64s * 8) as *const c::bkey_i
}

// ---- jset helpers ----

/// Total byte size of a jset including header.
pub fn jset_vstruct_bytes(jset: &c::jset) -> usize {
    let u64s = u32::from_le(jset.u64s) as usize;
    56 + u64s * 8
}

/// Number of sectors occupied by a jset on disk.
pub fn jset_vstruct_sectors(jset: &c::jset, block_bits: u16) -> usize {
    let bytes = jset_vstruct_bytes(jset);
    let block_size = 512usize << block_bits;
    ((bytes + block_size - 1) / block_size) * block_size >> 9
}

/// JSET_NO_FLUSH bitfield: bit 5 of le32 flags.
pub fn jset_no_flush(jset: &c::jset) -> bool {
    (u32::from_le(jset.flags) >> 5) & 1 != 0
}

// ---- vstruct iterators ----

/// Iterator over jset_entry references within a jset.
pub struct JsetEntryIter<'a> {
    cur: *const c::jset_entry,
    end: *const c::jset_entry,
    _phantom: PhantomData<&'a c::jset>,
}

impl<'a> Iterator for JsetEntryIter<'a> {
    type Item = &'a c::jset_entry;
    fn next(&mut self) -> Option<Self::Item> {
        if self.cur >= self.end {
            return None;
        }
        let entry = unsafe { &*self.cur };
        let next = unsafe { vstruct_next_entry(self.cur) };
        if next > self.end {
            return None;
        }
        self.cur = next;
        Some(entry)
    }
}

pub fn jset_entries(jset: &c::jset) -> JsetEntryIter<'_> {
    let start = jset.start.as_ptr();
    let end = unsafe { vstruct_last_jset(jset as *const c::jset) };
    JsetEntryIter { cur: start, end, _phantom: PhantomData }
}

/// Iterator over bkey_i references within a jset_entry.
pub struct JsetEntryKeyIter<'a> {
    cur: *const c::bkey_i,
    end: *const c::bkey_i,
    _phantom: PhantomData<&'a c::jset_entry>,
}

impl<'a> Iterator for JsetEntryKeyIter<'a> {
    type Item = &'a c::bkey_i;
    fn next(&mut self) -> Option<Self::Item> {
        if self.cur >= self.end {
            return None;
        }
        let k = unsafe { &*self.cur };
        if k.k.u64s == 0 {
            return None;
        }
        let next = unsafe { bkey_next_raw(self.cur) };
        if next > self.end {
            return None;
        }
        self.cur = next;
        Some(k)
    }
}

pub fn jset_entry_keys(entry: &c::jset_entry) -> JsetEntryKeyIter<'_> {
    let start = entry.start.as_ptr();
    let end = unsafe { vstruct_next_entry(entry as *const c::jset_entry) as *const c::bkey_i };
    JsetEntryKeyIter { cur: start, end, _phantom: PhantomData }
}

// ---- entry type conversion ----

/// Convert entry type byte to the enum, if it's a known type.
pub fn entry_type(entry: &c::jset_entry) -> Option<c::bch_jset_entry_type> {
    let raw = entry.type_ as u32;
    if raw < c::bch_jset_entry_type::BCH_JSET_ENTRY_NR as u32 {
        Some(unsafe { std::mem::transmute(raw) })
    } else {
        None
    }
}

/// Convert entry btree_id byte to the enum, if it's a known btree.
pub fn entry_btree_id(entry: &c::jset_entry) -> Option<c::btree_id> {
    let raw = entry.btree_id as u32;
    if raw < c::btree_id::BTREE_ID_NR as u32 {
        Some(unsafe { std::mem::transmute(raw) })
    } else {
        None
    }
}

// ---- jset_entry_log helpers ----

/// Get log message bytes from a jset_entry of type log.
/// Layout: jset_entry header (8 bytes) followed by d[] message bytes.
pub fn entry_log_msg(entry: &c::jset_entry) -> &[u8] {
    let msg_bytes = u16::from_le(entry.u64s) as usize * 8;
    if msg_bytes == 0 {
        return &[];
    }
    let ptr = entry as *const c::jset_entry as *const u8;
    let data = unsafe { std::slice::from_raw_parts(ptr.add(8), msg_bytes) };
    // Trim trailing nulls
    let len = data.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    &data[..len]
}

pub fn entry_log_str_eq(entry: &c::jset_entry, s: &str) -> bool {
    let msg = entry_log_msg(entry);
    msg.len() >= s.len() && &msg[..s.len()] == s.as_bytes()
}
