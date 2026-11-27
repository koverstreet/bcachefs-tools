// SPDX-License-Identifier: GPL-2.0

//! Journal entries written into a buffer - a jset's, the clean section's -
//! each whole, a u64 multiple, after the last: jset_entry_init() and
//! journal_entry_set(), bounded by the buffer.

use crate::format::{JSET_KEYS_U64s, JsetEntry, JsetEntryType};
use crate::types::ArmOf;
use crate::util::vstructs::{plain_bytes, plain_bytes_mut, Head, Plain};

/// No room left in the buffer for the entry.
#[derive(Debug)]
pub struct NoRoom;

pub struct EntryWriter<'a> {
    /* the buffer past what's written */
    room: &'a mut [u64],
    size: usize,
}

/// The u64s of an entry holding an @E, its header and @E: what push_entry()
/// writes.
pub const fn entry_u64s<E>() -> usize {
    JSET_KEYS_U64s + size_of::<E>() / size_of::<u64>()
}

/// An entry's header, of @ty, for @btree_id at @level, with @u64s of value:
/// journal_entry_init().
#[inline(always)]
fn header(ty: JsetEntryType, btree_id: u8, level: u8, u64s: usize) -> u64 {
    let mut e = Head::<JsetEntry>::default();
    e.type_ = ty.0 as u8;
    e.btree_id = btree_id;
    e.level = level;
    e.set_u64s(u64s);

    let mut h = 0u64;
    plain_bytes_mut(&mut h).copy_from_slice(plain_bytes(&e));
    h
}

impl<'a> EntryWriter<'a> {
    pub fn new(buf: &'a mut [u64]) -> Self {
        EntryWriter { size: buf.len(), room: buf }
    }

    /// The u64s written.
    pub fn len(&self) -> usize {
        self.size - self.room.len()
    }

    /// The u64s left to write.
    pub fn room(&self) -> usize {
        self.room.len()
    }

    /// The next @u64s, past what's written, for the caller to write all of.
    #[inline(always)]
    fn reserve(&mut self, u64s: usize) -> Result<&'a mut [u64], NoRoom> {
        if u64s > self.room.len() {
            return Err(NoRoom);
        }
        let (b, rest) = core::mem::take(&mut self.room).split_at_mut(u64s);
        self.room = rest;
        Ok(b)
    }

    /// An entry of @ty, for @btree_id at @level, of @u64s: its header
    /// written, and its value for the caller to write all of.
    #[inline(always)]
    pub fn add_entry(&mut self, ty: JsetEntryType, btree_id: u8, level: u8, u64s: usize)
        -> Result<&'a mut [u64], NoRoom>
    {
        let (hdr, v) = self.reserve(JSET_KEYS_U64s + u64s)?.split_at_mut(JSET_KEYS_U64s);
        hdr[0] = header(ty, btree_id, level, u64s);
        Ok(v)
    }

    /// The room left, filled with empty entries - of a header each - so
    /// what's written uses all of it: res_put()'s fill.
    pub fn pad(&mut self) {
        if !self.room.is_empty() {
            core::mem::take(&mut self.room).fill(header(JsetEntryType::btree_keys, 0, 0, 0));
        }
    }

    /// An entry of @ty, for @btree_id at @level, holding @data, zero padded to
    /// a u64: journal_entry_set().
    pub fn push_bytes(&mut self, ty: JsetEntryType, btree_id: u8, level: u8, data: &[u8])
        -> Result<(), NoRoom>
    {
        let v = plain_bytes_mut(self.add_entry(ty, btree_id, level, data.len().div_ceil(size_of::<u64>()))?);
        let (d, pad) = v.split_at_mut(data.len());
        d.copy_from_slice(data);
        pad.fill(0);
        Ok(())
    }

    /// @entries, whole entries already written, their counts set.
    pub fn push_entries(&mut self, entries: &[u64]) -> Result<(), NoRoom> {
        self.reserve(entries.len())?.copy_from_slice(entries);
        Ok(())
    }

    /// An entry of @e's type, for @btree_id at @level, holding @e:
    /// jset_entry_init(), filled in.
    pub fn push_entry<E: Plain + ArmOf<JsetEntry, Tag = u8>>(&mut self, btree_id: u8, level: u8, e: E)
        -> Result<(), NoRoom>
    {
        const { assert!(size_of::<E>() % size_of::<u64>() == 0) };
        let v = self.add_entry(JsetEntryType(E::TAG.into()), btree_id, level, size_of::<E>() / size_of::<u64>())?;
        plain_bytes_mut(v).copy_from_slice(plain_bytes(&e));
        Ok(())
    }
}
