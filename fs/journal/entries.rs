// SPDX-License-Identifier: GPL-2.0

//! Journal entries written into a buffer - a jset's, the clean section's -
//! each whole, a u64 multiple, after the last: jset_entry_init() and
//! journal_entry_set(), bounded by the buffer.

use core::mem::size_of;

use crate::c;
use crate::util::vstructs::{plain_bytes, Head, Plain, VStruct};

/// No room left in the buffer for the entry.
#[derive(Debug)]
pub struct NoRoom;

pub struct EntryWriter<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> EntryWriter<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        EntryWriter { buf, pos: 0 }
    }

    /// The bytes written.
    pub fn len(&self) -> usize {
        self.pos
    }

    /// The next @bytes of the buffer, zeroed, past what's written.
    fn reserve(&mut self, bytes: usize) -> Result<&mut [u8], NoRoom> {
        let b = self.buf.get_mut(self.pos..).and_then(|b| b.get_mut(..bytes)).ok_or(NoRoom)?;
        b.fill(0);
        self.pos += bytes;
        Ok(b)
    }

    /// An entry of @ty, for @btree_id at @level, holding @data - its parts,
    /// one after another, zero padded to a u64: journal_entry_set().
    pub fn push(&mut self, ty: c::bch_jset_entry_type, btree_id: u8, level: u8, data: &[&[u8]])
        -> Result<(), NoRoom>
    {
        let u64s = data.iter().map(|d| d.len()).sum::<usize>().div_ceil(size_of::<u64>());
        let mut e = Head::<c::jset_entry>::default();
        e.type_ = ty.0 as u8;
        e.btree_id = btree_id;
        e.level = level;
        e.set_val_u64s(u64s);

        let b = self.reserve(size_of::<Head<c::jset_entry>>() + u64s * size_of::<u64>())?;
        let (hdr, mut rest) = b.split_at_mut(size_of::<Head<c::jset_entry>>());
        hdr.copy_from_slice(plain_bytes(&e));
        for d in data {
            let (dst, r) = rest.split_at_mut(d.len());
            dst.copy_from_slice(d);
            rest = r;
        }
        Ok(())
    }

    /// @e, a whole entry of its type, its header's count set here:
    /// jset_entry_init(), filled in.
    pub fn push_entry<E: VStruct + Plain>(&mut self, mut e: E) -> Result<(), NoRoom> {
        const { assert!(size_of::<E>() % size_of::<u64>() == 0) };
        e.set_val_u64s(size_of::<E>() / size_of::<u64>() - 1);
        self.reserve(size_of::<E>())?.copy_from_slice(plain_bytes(&e));
        Ok(())
    }
}
