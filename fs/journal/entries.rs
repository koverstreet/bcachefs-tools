// SPDX-License-Identifier: GPL-2.0

//! Journal entries written into a buffer - a jset's, the clean section's -
//! each whole, a u64 multiple, after the last: jset_entry_init() and
//! journal_entry_set(), bounded by the buffer.

use core::mem::size_of;

use crate::c;
use crate::util::vstructs::{plain_bytes, Head, Plain, VStruct};
use zerocopy::IntoBytes;

/// No room left in the buffer for the entry.
#[derive(Debug)]
pub struct NoRoom;

pub struct EntryWriter<'a> {
    /* the buffer past what's written */
    room: &'a mut [u64],
    size: usize,
}

const HEADER_U64S: usize = size_of::<Head<c::jset_entry>>() / size_of::<u64>();

/// An entry's header, of @ty, for @btree_id at @level, with @u64s of value:
/// journal_entry_init().
#[inline(always)]
fn header(ty: c::bch_jset_entry_type, btree_id: u8, level: u8, u64s: usize) -> u64 {
    let mut e = Head::<c::jset_entry>::default();
    e.type_ = ty.0 as u8;
    e.btree_id = btree_id;
    e.level = level;
    e.set_val_u64s(u64s);

    let mut h = 0u64;
    h.as_mut_bytes().copy_from_slice(plain_bytes(&e));
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

    /// An entry of @ty, for @btree_id at @level, of @u64s: its header written,
    /// and its value for the caller to write all of -
    /// __bch2_journal_add_entry().
    #[inline(always)]
    pub fn add_entry(&mut self, ty: c::bch_jset_entry_type, btree_id: u8, level: u8, u64s: usize)
        -> Result<&'a mut [u64], NoRoom>
    {
        let (hdr, v) = self.reserve(HEADER_U64S + u64s)?.split_at_mut(HEADER_U64S);
        hdr[0] = header(ty, btree_id, level, u64s);
        Ok(v)
    }

    /// The room left, filled with empty entries - of a header each - so
    /// what's written uses all of it: bch2_journal_res_put()'s fill.
    pub fn pad(&mut self) {
        if !self.room.is_empty() {
            core::mem::take(&mut self.room).fill(header(c::bch_jset_entry_type::btree_keys, 0, 0, 0));
        }
    }

    /// An entry of @ty, for @btree_id at @level, holding @data, zero padded to
    /// a u64: journal_entry_set().
    pub fn push_bytes(&mut self, ty: c::bch_jset_entry_type, btree_id: u8, level: u8, data: &[u8])
        -> Result<(), NoRoom>
    {
        let v = self.add_entry(ty, btree_id, level, data.len().div_ceil(size_of::<u64>()))?.as_mut_bytes();
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

    /// @e, a whole entry of its type, its header's count set here:
    /// jset_entry_init(), filled in.
    pub fn push_entry<E: VStruct + Plain>(&mut self, mut e: E) -> Result<(), NoRoom> {
        const { assert!(size_of::<E>() % size_of::<u64>() == 0) };
        e.set_val_u64s(size_of::<E>() / size_of::<u64>() - HEADER_U64S);
        self.reserve(size_of::<E>() / size_of::<u64>())?.as_mut_bytes().copy_from_slice(plain_bytes(&e));
        Ok(())
    }
}
