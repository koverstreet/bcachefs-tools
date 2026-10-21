// SPDX-License-Identifier: GPL-2.0

//! The btree roots, as journal entries: what every journal write, and the
//! clean section, records of where each btree's root is.

use core::ffi::c_ulong;
use core::mem::size_of;

use cstruct_macros::c_export;
use zerocopy::IntoBytes;

use crate::c;
use crate::fs::Fs;
use crate::journal::entries::{EntryWriter, NoRoom};
use crate::locking::{Guard, Lock};
use crate::util::ffi::Opaque;
use crate::util::vstructs::{plain_bytes, Head};

/// The live roots, but those whose bit is set in @skip - a btree past bit 63
/// can't be. Under root_lock, which owns their on-disk fields: key, level,
/// alive.
fn roots<'a>(fs: &'a Fs, _root_lock: &'a Guard<'_, c::mutex>, skip: u64)
    -> impl Iterator<Item = (u32, &'a c::btree_root)>
{
    (0..fs.btree_id_nr_alive())
        .filter(move |&id| id >= u64::BITS || skip & (1 << id) == 0)
        .filter_map(move |id| fs.btree_root(id).filter(|r| r.alive != 0).map(|r| (id, r)))
}

/// @r's key: its bkey_i, and its value on into key_pad, as long as it says.
fn key_parts(r: &c::btree_root) -> [&[u8]; 2] {
    let bytes = usize::from(r.key.k.u64s) * size_of::<u64>();
    let head = plain_bytes(&r.key);
    [head, &r.key_pad.as_bytes()[..bytes - head.len()]]
}

/// An entry's bytes, for @r's key.
fn entry_bytes(r: &c::btree_root) -> usize {
    size_of::<Head<c::jset_entry>>() + usize::from(r.key.k.u64s) * size_of::<u64>()
}

fn push_roots(fs: &Fs, root_lock: &Guard<'_, c::mutex>, skip: u64, w: &mut EntryWriter<'_>)
    -> Result<(), NoRoom>
{
    for (id, r) in roots(fs, root_lock, skip) {
        w.push(c::bch_jset_entry_type::BCH_JSET_ENTRY_btree_root, id as u8, r.level, &key_parts(r))?;
    }
    Ok(())
}

/// Each live btree root as a btree_root journal entry, but those in @skip.
pub fn btree_roots_to_journal_entries(fs: &Fs, w: &mut EntryWriter<'_>, skip: u64) -> Result<(), NoRoom> {
    push_roots(fs, &Lock::from_c(&fs.btree.cache.root_lock).lock(), skip, w)
}

/// btree_roots_to_journal_entries(), for C's journal write: at @end, the end
/// of what's written, which it returns.
///
/// # Safety
/// There's room at @end for an entry for every live root.
#[c_export]
pub unsafe fn bch2_btree_roots_to_journal_entries(c: &Opaque<c::bch_fs>, end: *mut Head<c::jset_entry>,
                                                  skip: c_ulong) -> *mut Head<c::jset_entry> {
    let fs = Fs::from_c(c);
    let root_lock = Lock::from_c(&fs.btree.cache.root_lock).lock();
    let bytes = roots(fs, &root_lock, skip.into()).map(|(_, r)| entry_bytes(r)).sum();

    // SAFETY: the caller's - room for these
    let mut w = EntryWriter::new(unsafe { core::slice::from_raw_parts_mut(end.cast(), bytes) });
    push_roots(fs, &root_lock, skip.into(), &mut w).expect("sized for them, under the same lock");
    // SAFETY: as long as what was written
    unsafe { end.byte_add(bytes) }
}
