// SPDX-License-Identifier: GPL-2.0

//! The damage btree: a persistent record of which inodes were damaged by
//! errors and repairs.
//!
//! A damage key is an inode version's - one per (0, inum, snapshot), at that
//! position - and its value the same records the superblock's errors
//! section keeps, in the same vocabulary as the error counters and fsck:
//! per bch_sb_error_id, an occurrence count and the times of the first and
//! last occurrence (bch_sb_field_error_entry_v2), strictly ascending by id.
//! A count saturates, and a value holds what a key's u8 of u64s does,
//! MAX_ENTRIES: past that, new ids aren't recorded.
//!
//! A record is written in the same transaction as the repair that does the
//! damage: a crash can't separate the two, and a restart discards both. An
//! inode version sees its own record and its ancestor versions': damage
//! done to an ancestor's data is damage to what a descendant sees. A cleared
//! record is a whiteout where an older version still has one, hiding it
//! from this view and its descendants.
//!
//! A damage key has an inode at its exact position: records are written at
//! repaired keys' positions - inode keys, or content keys, and a content key
//! at snapshot X implies an inode at X - and inode removal deletes the two
//! together. check_damage deletes a key without one: left by a kernel
//! without the damage btree, it would claim a reused inum's history.
//!
//! The in-memory fsck damage report, error::fsck_damaged()'s, dies with the
//! mount; this is its durable counterpart. Extents lost with a btree node
//! are stashed, by range, where the node's lost, and attributed to inodes
//! later, when the inodes btree can be walked: lost extents below.

use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{spos, BkeySC, POS_MIN, SPOS_MAX};
use crate::btree::iter::{BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, TransAttempt, TransBkey, UpdateTriggerFlags};
use crate::c;
use crate::errcode::{ret_to_c, BchError};
use crate::fs::Fs;
use crate::init::error::{id, BkeyValidate};
use crate::inode::{self, bkey_is_inode};
use crate::sb::errors::{merge, prt_error_nr, ErrorEntry};
use crate::snapshots::subvolume;
use crate::util::{DateTime, Printbuf};
use crate::util::alloc::{flags::GFP_KERNEL, kvvec_insert, KVVec};
use crate::util::ffi::Opaque;
use crate::util::kernel::ktime_get_real_seconds;
use crate::util::vstructs::Validated;
use crate::{bch_err, bch_err_ratelimited, bkey_fsck_err_on, btree_trans, fsck_err_on, prt};
use c::bch_sb_field_error_entry_v2 as Entry;
use core::cmp::Ordering;
use core::ffi::c_int;
use core::mem::size_of;
use cstruct_macros::c_export;

// ── The key ──────────────────────────────────────────────────────────────

const ENTRY: usize = size_of::<Entry>();

/// The most entries a key's value holds: its u64s are a u8.
const MAX_ENTRIES: usize = c::BKEY_VAL_U64s_MAX * size_of::<u64>() / ENTRY;

fn validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
    let val_bytes = v.k.k.val_bytes();
    bkey_fsck_err_on!(v, val_bytes % ENTRY != 0,
                      id::damage_entries_bad, "val size {val_bytes} not a multiple of entry size")?;

    let entries = &v.k.arm::<c::Damage>().expect("a damage key").v().entries;
    bkey_fsck_err_on!(v, entries.is_empty(), id::damage_entries_bad, "no entries")?;

    let mut prev = 0;
    for (i, e) in entries.iter().enumerate() {
        let id = e.id().0;
        // strictly ascending ids: covers unsorted, dup and zero
        bkey_fsck_err_on!(v, id <= prev, id::damage_entries_bad,
                          "entry {i}: id {id} nr {} (prev id {prev})", e.nr())?;
        prev = id;

        bkey_fsck_err_on!(v, e.first() > e.last(), id::damage_entries_bad,
                          "entry {i}: first occurrence after last")?;
    }
    Ok(())
}

#[c_export]
pub fn bch2_damage_validate(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, from: &c::bkey_validate_context)
    -> core::ffi::c_int
{
    ret_to_c(validate(&BkeyValidate { fs: Fs::from_c(c), k, from }))
}

#[cold]
#[c_export]
pub fn bch2_damage_to_text(out: &mut Printbuf, _c: &Opaque<c::bch_fs>, k: BkeySC<'_>) {
    for (i, e) in k.arm::<c::Damage>().expect("a damage key").v().entries.iter().enumerate() {
        if i != 0 {
            out.newline();
        }
        prt!(out, "{} nr ", e.id());
        prt_error_nr(out, e.nr());
        prt!(out, " first {} last {}", DateTime(e.first() as i64), DateTime(e.last() as i64));
    }
}

// ── Reading and writing records ──────────────────────────────────────────

/// Whether inode @inum has damage recorded in @snapshot or an ancestor
/// version - the version @snapshot sees.
pub fn inode_has_damage(t: &TransAttempt<'_, '_>, inum: u64, snapshot: u32) -> Result<bool, BchError> {
    let mut iter = BtreeIter::new(t, c::btree_id::damage, spos(0, inum, snapshot), BtreeIterFlags::NOPRESERVE);
    Ok(iter.peek_slot(t)?.is_some_and(|k| k.k.key_type() == c::bch_bkey_type::KEY_TYPE_damage))
}

/// Inode @inum's damage as @snapshot sees it - its records and its ancestor
/// versions', merged, sorted by id: damage recorded against an ancestor
/// version happened to the data a descendant sees.
pub fn accumulate(t: &TransAttempt<'_, '_>, inum: u64, mut snapshot: u32)
    -> Result<KVVec<c::bch_sb_error_entry_cpu>, BchError>
{
    let mut out: KVVec<c::bch_sb_error_entry_cpu> = KVVec::new();
    loop {
        let mut iter = BtreeIter::new(t, c::btree_id::damage, spos(0, inum, snapshot), BtreeIterFlags::NOPRESERVE);
        let Some(d) = iter.peek_slot(t)?.and_then(|k| k.arm::<c::Damage>()) else { break };

        for e in d.v().entries.iter() {
            match out.binary_search_by_key(&e.id().0, |o| o.id().into()) {
                Ok(j)  => out[j] = merge(out[j], e.into()),
                Err(j) => kvvec_insert(&mut out, j, e.into())?,
            }
        }

        match t.fs().snapshots.parent(d.k.p.snapshot) {
            Some(parent) if parent != 0 => snapshot = parent,
            _ => break,
        }
    }
    Ok(out)
}

/// Record that @err damaged the inode at @pos, in @t: the record commits
/// with the repair that does the damage, so a crash can't separate the two,
/// and a restart discards the count with the repair it belonged to. @pos is
/// a content key's, or an inode key's at (0, inum, snapshot): a zero inode
/// field means the inum's in the offset.
///
/// Also counted in the in-memory damaged paths report - the end of fsck's
/// summary, debugfs's fsck_damaged_paths - the reporting surface until
/// reports read the damage btree. In -n mode the update never commits while
/// the report still has it: as it should.
pub fn record(t: &TransAttempt<'_, '_>, pos: c::bpos, err: c::bch_sb_error_id) -> Result<(), BchError> {
    let at = spos(0, if pos.inode != 0 { pos.inode } else { pos.offset }, pos.snapshot);

    crate::init::error::fsck_damaged(t.fs(), pos, err);

    let mut iter = BtreeIter::new(t, c::btree_id::damage, at,
                                  BtreeIterFlags::ALL_SNAPSHOTS | BtreeIterFlags::INTENT);
    let cur = iter.peek_slot(t)?.and_then(|k| k.arm::<c::Damage>());
    let old = cur.as_ref().map_or(&[][..], |d| &d.v().entries);
    let (idx, found) = match old.binary_search_by_key(&err.0, |e| e.id().0) {
        Ok(i)  => (i, true),
        Err(i) => (i, false),
    };

    /*
     * A key's u64s is a u8: a value grown past the most it holds would wrap
     * it, corrupting the key in the same transaction as a repair. A new id
     * once full isn't recorded; the in-memory report above still has it.
     */
    let nr = old.len() + usize::from(!found);
    if nr > MAX_ENTRIES {
        return Ok(());
    }

    let now = ktime_get_real_seconds() as u64;
    let mut n = t.bkey_alloc_init::<c::Damage>(nr * ENTRY / size_of::<u64>(), at)?;
    let dst = &mut n.v_mut().entries;
    if found {
        dst.copy_from_slice(old);
        let e = dst[idx];
        dst[idx] = Entry::new(e.id(), e.nr() + 1, e.first(), now);
    } else {
        dst[..idx].copy_from_slice(&old[..idx]);
        dst[idx + 1..].copy_from_slice(&old[idx..]);
        dst[idx] = Entry::new(err, 1, now, now);
    }

    t.update(&iter, &n, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)
}

/// record(), for a key at @pos in @btree: damage is keyed by inode, and only
/// an extents position names one - getting to an indirect extent's inodes
/// would mean walking reflink pointers backwards.
pub fn record_key(t: &TransAttempt<'_, '_>, btree: c::btree_id, pos: c::bpos, err: c::bch_sb_error_id)
    -> Result<(), BchError>
{
    if btree == c::btree_id::extents { record(t, pos, err) } else { Ok(()) }
}

fn inode_exists(t: &TransAttempt<'_, '_>, inum: u64, snapshot: u32) -> Result<bool, BchError> {
    let mut iter = BtreeIter::new(t, c::btree_id::inodes, spos(0, inum, snapshot), BtreeIterFlags::ALL_SNAPSHOTS);
    Ok(iter.peek_slot(t)?.is_some_and(|k| bkey_is_inode(k.k)))
}

/// record_key(), for damage found after the fact, when the file may be gone
/// too - a journal rewind can take its creation with it: a damage key for an
/// inode that doesn't exist would only be check_damage's to delete.
pub fn record_key_if_inode(t: &TransAttempt<'_, '_>, btree: c::btree_id, pos: c::bpos,
                           err: c::bch_sb_error_id) -> Result<(), BchError> {
    if btree != c::btree_id::extents || !inode_exists(t, pos.inode, pos.snapshot)? {
        return Ok(());
    }
    record(t, pos, err)
}

/// Data lost or corrupted outside fsck_err()'s reporting - a device's
/// removal dropping the last replica, a read error: @err counted in the
/// superblock, and record_key().
pub fn record_data_loss(t: &TransAttempt<'_, '_>, btree: c::btree_id, pos: c::bpos,
                        err: c::bch_sb_error_id) -> Result<(), BchError> {
    t.fs().sb_error_count(err);
    record_key(t, btree, pos, err)
}

/// Inode @inum in @snapshot is being deleted: delete its damage record for
/// this version, or the whiteout a cleared one left, in the same
/// transaction. Damage at ancestor versions is theirs, reaped when they are,
/// or merged down by snapshot deletion. Most inodes have none, so nothing's
/// queued for a key that isn't there.
pub fn delete(t: &TransAttempt<'_, '_>, inum: u64, snapshot: u32) -> Result<(), BchError> {
    use c::bch_bkey_type as T;

    let mut iter = BtreeIter::new(t, c::btree_id::damage, spos(0, inum, snapshot), BtreeIterFlags::ALL_SNAPSHOTS);
    let ty = iter.peek_slot(t)?.map(|k| k.k.key_type());
    if ty != Some(T::KEY_TYPE_damage) && ty != Some(T::KEY_TYPE_whiteout) {
        return Ok(());
    }
    t.delete_at(&iter, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)
}

/// Clear @inum's damage record in its subvolume's view: a filtered delete,
/// so a real one when no older version needs the record and a whiteout when
/// one does - snapshots keep their view, and the whiteout hides inherited
/// damage from this view and its descendants. A clean file's is nothing.
pub fn clear(t: &TransAttempt<'_, '_>, inum: c::subvol_inum) -> Result<(), BchError> {
    let snapshot = subvolume::get_snapshot(t, inum.subvol as u32)?;

    let mut iter = BtreeIter::new(t, c::btree_id::damage, spos(0, inum.inum, snapshot), BtreeIterFlags::INTENT);
    if !iter.peek_slot(t)?.is_some_and(|k| k.k.key_type() == c::bch_bkey_type::KEY_TYPE_damage) {
        return Ok(());
    }
    t.delete_at(&iter, UpdateTriggerFlags::empty())
}

/// Damage keys @a and @b merged into one at @pos, in @t's memory: their
/// entries' sorted union, occurrence counts summed, first times the earlier
/// and last the later. For a dying snapshot's key collapsing into its live
/// child's: the dying key can hold counts recorded after the child's was
/// made, so copying it only if the child has none would lose them. Entries
/// past a key's most are dropped, the lowest ids kept, as record() skips
/// new ones once full.
pub fn keys_merge<'a, 't>(
    t:   &TransAttempt<'a, 't>,
    pos: c::bpos,
    a:   BkeySC<'_>,
    b:   BkeySC<'_>,
) -> Result<TransBkey<'a, 't, c::Damage<Validated>>, BchError> {
    let ea = &a.arm::<c::Damage>().expect("a damage key").v().entries;
    let eb = &b.arm::<c::Damage>().expect("a damage key").v().entries;

    let nr = (ea.len() + eb.len()).min(MAX_ENTRIES);
    let mut n = t.bkey_alloc_init::<c::Damage>(nr * ENTRY / size_of::<u64>(), pos)?;

    let (mut i, mut j, mut out) = (0, 0, 0);
    let dst = &mut n.v_mut().entries;
    while (i < ea.len() || j < eb.len()) && out < dst.len() {
        let id_a = ea.get(i).map_or(u32::MAX, |e| e.id().0);
        let id_b = eb.get(j).map_or(u32::MAX, |e| e.id().0);

        dst[out] = match id_a.cmp(&id_b) {
            Ordering::Equal => {
                let (x, y) = (&ea[i], &eb[j]);
                (i, j) = (i + 1, j + 1);
                Entry::new(x.id(), x.nr() + y.nr(),
                                                    x.first().min(y.first()), x.last().max(y.last()))
            }
            Ordering::Less    => { i += 1; ea[i - 1] }
            Ordering::Greater => { j += 1; eb[j - 1] }
        };
        out += 1;
    }

    n.k_mut().set_val_bytes(out * ENTRY);
    Ok(n)
}

/* For C's: in the attempt C has in progress */

#[c_export]
pub extern "C" fn bch2_damage_record(trans: &BtreeTrans<'_>, pos: c::bpos, err: c::bch_sb_error_id) -> c_int {
    ret_to_c(record(&unsafe { trans.attempt_in_progress() }, pos, err))
}

#[c_export]
pub extern "C" fn bch2_damage_record_key(trans: &BtreeTrans<'_>, btree: c::btree_id, pos: c::bpos,
                                     err: c::bch_sb_error_id) -> c_int {
    ret_to_c(record_key(&unsafe { trans.attempt_in_progress() }, btree, pos, err))
}

#[c_export]
pub extern "C" fn bch2_damage_record_key_if_inode(trans: &BtreeTrans<'_>, btree: c::btree_id, pos: c::bpos,
                                              err: c::bch_sb_error_id) -> c_int {
    ret_to_c(record_key_if_inode(&unsafe { trans.attempt_in_progress() }, btree, pos, err))
}

// ── Lost extents ─────────────────────────────────────────────────────────

/// Next to bch2_btree_lost_data() at the sites that lose a node: @k is the
/// pointer its parent had, so the range is known even when nothing of the
/// node could be read. Stashed, not attributed: attributing walks the inodes
/// btree, and these sites can't - they're mid-traverse holding the node's
/// locks, or have the dangling pointer's deletion queued. check_extents,
/// which bch2_btree_lost_data() schedules, attributes them.
///
/// A v1 btree_ptr has no min_key: the range would be a guess, and damage on
/// the wrong file is worse than none. Extents only, so a lost reflink node
/// is silent: its range names indirect extents, and the files that lost
/// data are those pointing into it - backpointers run bucket to extent, not
/// back, so finding them would be a scan of the extents btree for reflink
/// pointers in the range: an fsck pass, not something a loss site can do.
pub fn note_lost_extents(fs: &Fs, btree: c::btree_id, k: BkeySC<'_>) {
    if btree != c::btree_id::extents {
        return;
    }
    let Some(p) = k.arm::<c::BtreePtrV2>() else { return };
    let (start, end) = (p.v().min_key, k.k.p);

    if fs.lost_extents_ranges.lock().push(start..end, GFP_KERNEL).is_err() {
        bch_err!(fs, "couldn't record lost extents {}:{}-{}:{}",
                 { start.inode }, { start.offset }, { end.inode }, { end.offset });
    }
}

/// The stashed ranges attributed, to the inodes in them that had data:
/// bi_sectors says so, from the inodes btree, which didn't lose the node -
/// an inode that lost every extent is otherwise a sparse file. Inexact only
/// at the ends: an inode straddling one may have kept what it cares about,
/// one strictly inside lost all of it. Before check_extents' walk, to which
/// such an inode looks sparse.
pub fn record_lost_extents(fs: &Fs) {
    let ranges = core::mem::take(&mut *fs.lost_extents_ranges.lock());
    if ranges.is_empty() {
        return;
    }

    let trans = btree_trans!(fs);
    let flags = BtreeIterFlags::ALL_SNAPSHOTS | BtreeIterFlags::PREFETCH;
    let mut failed = 0;

    for r in ranges.iter() {
        let mut iter = BtreeIter::new(&trans, c::btree_id::inodes, spos(0, r.start.inode, 0), flags);
        let ret = iter.for_each_max_commit(&trans, spos(0, r.end.inode, u32::MAX), flags, None,
                                           CommitFlags::NO_ENOSPC, |t, _, k| {
            if !bkey_is_inode(k.k) || inode::unpack(fs, k).bi_sectors == 0 {
                return Ok(());
            }
            // not record_data_loss(), which narrows on the btree a position
            // is from: this one's an inode key's
            fs.sb_error_count(id::data_lost_btree_node_read_error);
            record(t, k.k.p, id::data_lost_btree_node_read_error)
        });

        // one range we can't attribute mustn't take the others down
        if let Err(e) = ret {
            failed += 1;
            bch_err_ratelimited!(fs, "error attributing lost extents {}:{}-{}:{}: {}",
                                 { r.start.inode }, { r.start.offset }, { r.end.inode }, { r.end.offset }, e);
        }
    }

    // the ratelimit hides how much was lost, and these fail together when
    // they fail at all - one cause, every range: say the count
    if failed != 0 {
        bch_err!(fs, "couldn't attribute {failed} of {} lost extents ranges", ranges.len());
    }
}

#[c_export]
pub fn bch2_damage_note_lost_extents(c: &Opaque<c::bch_fs>, btree: c::btree_id, k: BkeySC<'_>) {
    note_lost_extents(Fs::from_c(c), btree, k)
}

// ── check_damage ─────────────────────────────────────────────────────────

/// Damage keys without their inodes: see the top of the file.
fn check_damage(fs: &Fs) -> Result<(), BchError> {
    let trans = btree_trans!(fs);
    let flags = BtreeIterFlags::ALL_SNAPSHOTS | BtreeIterFlags::PREFETCH;
    let mut iter = BtreeIter::new(&trans, c::btree_id::damage, POS_MIN, flags);
    iter.for_each_max_commit(&trans, SPOS_MAX, flags, None, CommitFlags::NO_ENOSPC, |t, iter, k| {
        if k.k.key_type() == c::bch_bkey_type::KEY_TYPE_damage &&
           fsck_err_on!(t, !inode_exists(t, k.k.p.offset, k.k.p.snapshot)?, id::damage_key_no_inode,
                        "damage key with no inode:\n{}", k.to_text(fs))? {
            t.delete_at(iter, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
        }
        Ok(())
    })
}

crate::recovery_pass!(bch2_check_damage => check_damage);
