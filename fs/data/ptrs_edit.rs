//! Editing a key's entries in place - a PtrsMut key's, whose entries run to
//! its value's end: dropping an entry, or a pointer and what was only its, and
//! inserting one into a TransBkey's room. By EntryPos, the walk's positions,
//! each good until something before it moves.

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

    /// Drop the pointer at @at and what was only its: the stripe_ptr before
    /// it, and the crc before it unless a pointer after it, before another
    /// crc, still has it. bch2_bkey_drop_ptr_noerror(), of a key whose
    /// pointers can be edited.
    fn drop_ptr_entries(&mut self, fs: &Fs, at: EntryPos) {
        assert_eq!(self.extent_entries_at(fs).find(|(p, _)| *p == at).map(|(_, e)| EntryKind::of(e)),
                   Some(EntryKind::Ptr), "dropping the pointer at u64 {}, where there isn't one", at.0);

        let drop_crc = self.extent_entries_at(fs)
            .skip_while(|(p, _)| *p <= at)
            .map(|(_, e)| EntryKind::of(e))
            .find(|k| matches!(k, EntryKind::Crc | EntryKind::Ptr))
            != Some(EntryKind::Ptr);

        self.drop_entry(fs, at);

        let mut cur = at;
        while let Some((p, kind)) = entry_before(&*self, fs, cur) {
            match kind {
                EntryKind::Ptr => break,
                EntryKind::Crc if drop_crc => self.drop_entry(fs, p),
                EntryKind::Stripe => self.drop_entry(fs, p),
                _ => {}
            }
            cur = p;
        }
    }

    /// Drop the stripe_ptr of the first pointer to @dev - the last one
    /// before it, as C has it: bch2_bkey_drop_ec().
    ///
    /// XXX: C's - the stripe_ptr isn't forgotten at each pointer, so a
    /// pointer to @dev without its own would drop an earlier pointer's; and
    /// with none at all, C dereferences NULL, where this panics. Its callers
    /// pass devices with a stripe; for ec's conversion.
    fn drop_ec(&mut self, fs: &Fs, dev: u32) {
        let mut ec = None;
        let mut found = false;
        for (p, e) in self.extent_entries_at(fs) {
            match e {
                ExtentEntry::stripe_ptr(_) => ec = Some(p),
                ExtentEntry::ptr(ptr) if u32::from(ptr.dev()) == dev => {
                    found = true;
                    break;
                }
                _ => {}
            }
        }
        if found {
            let ec = ec.unwrap_or_else(|| panic!("dropping the stripe_ptr of the pointer to dev {dev}, with none"));
            self.drop_entry(fs, ec);
        }
    }

    /// drop_ec() for each pointer of @mask, by its index among the pointers:
    /// bch2_bkey_drop_ec_mask(). Panics on a bit past the last pointer, where
    /// C loops forever.
    fn drop_ec_mask(&mut self, fs: &Fs, mut mask: u32) {
        while mask != 0 {
            let (bit, dev) = self.ptrs(fs).enumerate()
                .find(|(i, _)| *i < 32 && mask & (1 << i) != 0)
                .map(|(i, p)| (1u32 << i, u32::from(p.dev())))
                .unwrap_or_else(|| panic!("dropping stripe_ptrs by mask {mask:#x}, past the key's pointers"));
            self.drop_ec(fs, dev);
            mask &= !bit;
        }
    }
}

impl<K: BkeyVMut<V = PtrsMut>> BkeyVPtrsMut for K {}

/// What an entry is, for the editing that drops what belongs to a pointer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EntryKind {
    Ptr,
    Crc,
    Stripe,
    Other,
}

impl EntryKind {
    fn of(e: ExtentEntry<'_>) -> Self {
        match e {
            ExtentEntry::ptr(_) => Self::Ptr,
            ExtentEntry::crc32(_) | ExtentEntry::crc64(_) | ExtentEntry::crc128(_) => Self::Crc,
            ExtentEntry::stripe_ptr(_) => Self::Stripe,
            _ => Self::Other,
        }
    }
}

/// @k's last entry before @at, and what kind it is: extent_entry_prev().
fn entry_before<K: BkeyV<V = PtrsMut>>(k: &K, fs: &Fs, at: EntryPos) -> Option<(EntryPos, EntryKind)> {
    k.extent_entries_at(fs)
        .take_while(|(p, _)| *p < at)
        .last()
        .map(|(p, e)| (p, EntryKind::of(e)))
}

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

    /// Its extent flags made @flags: its flags entry's, which comes first, or
    /// one inserted first - NoRoom if the buffer can't hold it. The caller
    /// requests the extent_flags incompat feature. bch2_bkey_extent_flags_set().
    pub fn extent_flags_set(&mut self, fs: &Fs, flags: u64) -> Result<(), NoRoom> {
        // the field's 57 bits, as C's bitfield assignment truncates to
        let flags = flags & ((1 << 57) - 1);

        if let Some(ExtentEntryMut::flags(f)) = self.extent_entries_mut(fs).next() {
            f.set_flags(flags);
            return Ok(());
        }

        let start = EntryPos(PtrsMut::entry_range(self.as_sc()).start / size_of::<u64>());
        self.insert_entry(start, &[c::bch_extent_flags::new().with_flags(flags).into_bits()])
    }
}
