//! Editing a key's entries in place - a PtrsMut key's, whose entries run to
//! its value's end: dropping an entry, and inserting one into a TransBkey's
//! room. By EntryPos, the walk's positions, each good until something before
//! it moves.

use super::*;
use crate::btree::bkey::BKEY_U64S;
use crate::btree::iter::{NoRoom, TransBkey};

/// What a PtrsMut key does, in whichever form it's held: dropping only
/// shrinks it, so any mutable key can; inserting grows it, so that's
/// TransBkey's.
pub trait BkeyVPtrsMut: BkeyVMut<V = PtrsMut> {
    /// Where an entry inserted last goes: the value's end.
    fn entries_end(&self) -> EntryPos {
        EntryPos(self.k().val_bytes() / size_of::<u64>())
    }

    /// Drop the entry at @at, whatever it is, the entries after it moving
    /// down: bch2_bkey_extent_entry_drop(). Panics if no entry starts at @at,
    /// or the one that does is where the walk stopped.
    fn drop_entry(&mut self, fs: &Fs, at: EntryPos) {
        let len = match self.extent_entries_at(fs).find(|(p, _)| *p == at) {
            None => panic!("no entry of the key at u64 {} to drop", at.0),
            Some((_, e @ (ExtentEntry::Invalid { .. } | ExtentEntry::PastEnd { .. }))) =>
                panic!("dropping the entry at u64 {}, of type {}, that the walk can't step past", at.0, e.ty()),
            Some((_, e)) => e.bytes().len(),
        };
        let start = at.0 * size_of::<u64>();
        self.val_bytes_mut().copy_within(start + len.., start);
        self.as_s().k.u64s -= (len / size_of::<u64>()) as u8;
    }
}

impl<K: BkeyVMut<V = PtrsMut>> BkeyVPtrsMut for K {}

impl TransBkey<'_, '_, PtrsMut> {
    /// Insert @entry - an extent entry, as its u64s - at @at, the entries from
    /// there moving up: __extent_entry_insert(); at entries_end(), appended.
    /// NoRoom if the key's buffer can't hold it.
    pub fn insert_entry(&mut self, at: EntryPos, entry: &[u64]) -> Result<(), NoRoom> {
        let val_u64s = self.entries_end().0;
        assert!(at.0 <= val_u64s, "inserting at u64 {} of a value of {val_u64s}", at.0);

        self.set_val_u64s(val_u64s + entry.len())?;
        let val = &mut self.as_mut_u64s()[BKEY_U64S..][..val_u64s + entry.len()];
        val.copy_within(at.0..val_u64s, at.0 + entry.len());
        val[at.0..][..entry.len()].copy_from_slice(entry);
        Ok(())
    }
}
