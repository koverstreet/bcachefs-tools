// SPDX-License-Identifier: GPL-2.0

//! Subvolumes: independent directory trees, each with a root inode and a
//! snapshot, the leaf its writes go to.
//!
//! State is live, unlinked or deleted, stored as codewords so check_subvol()
//! can correct a damaged field to the nearest one. set_state() also writes
//! the legacy UNLINKED_OBSOLETE flag; a key with state 0 predates the field,
//! and the flag is its state.
//!
//! unlink() marks a subvolume unlinked and, on commit, queues the delete work,
//! which waits for the VFS to drop the subvolume's inodes and then calls
//! set_deleted(). A deleted subvolume is a tombstone until snapshot deletion
//! reaps its snapshot: get() returns ENOENT for it, get_snapshot() doesn't.
//! check_subvols deletes subvolumes a crash left unlinked.
//!
//! subvolume_children has a bit at (fs_path_parent, subvolume) for each nested
//! subvolume, maintained by the trigger.

use crate::btree::bkey::{pos, BkeyI, BkeySC, SizedBkey, POS_MIN};
use crate::util::vstructs::{Validated, Whole};
use crate::btree::bkey_methods::TriggerOp;
use crate::btree::cow_key::CowKey;
use crate::btree::iter::{
    commit_do, is_restart, lockrestart_do, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags,
    LoopControl, TransAttempt, UpdateTriggerFlags,
};
use crate::c;
use crate::c::bch_recovery_pass::*;
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::fs::{Fs, WriteRef};
use crate::init::error::{count_fsck_err, id, BkeyValidate};
use crate::init::passes;
use crate::inode;
use crate::snapshots::snapshot;
use crate::snapshot_states::SUBVOLUME_STATE_VALUES;
use crate::util::alloc::KVVec;
use crate::types::RustOnly;
use crate::util::ffi::Opaque;
use crate::util::sync::new_mutex;
use crate::util::Printbuf;
use crate::{bch_err, bch_err_msg, bch_notice, bkey_fsck_err_on, fs_inconsistent, fsck_err,
            fsck_err_on, fsck_err_report};
use core::fmt;
use core::mem::offset_of;
use cstruct_macros::c_export;

/// Whether subvolume key @k is long enough to have @field.
macro_rules! has_field {
    ($k:expr, $field:ident) => {
        $k.val_bytes().len() >=
            offset_of!(c::Subvolume, repr.$field) + size_of_val(&c::Subvolume::default().$field)
    };
}

// ── Lookups ──────────────────────────────────────────────────────────────

/// Report @subvol missing, and schedule check_inodes to recreate it.
#[cold]
fn missing(fs: &Fs, subvol: u32) -> Result<(), BchError> {
    let mut msg = Printbuf::new();
    writeln!(msg, "missing subvolume {subvol}");
    let log = count_fsck_err(fs, id::subvol_missing, &mut msg);
    msg.set_suppressed(!log);

    let ret = passes::run_explicit(fs, &mut msg, BCH_RECOVERY_PASS_check_inodes,
                                   c::bch_run_recovery_pass_flags::from_bits_retain(0));
    if !msg.is_suppressed() {
        bch_err!(fs, "{}", msg);
    }
    ret
}

/// A tombstone is ENOENT_subvolume_deleted.
pub fn get_key(
    t:                         &TransAttempt<'_, '_>,
    subvol:                    u32,
    inconsistent_if_not_found: bool,
) -> Result<BkeyI<c::Subvolume>, BchError> {
    let fs = t.fs();

    let mut ret = t.bkey_get_key::<c::Subvolume>(c::btree_id::subvolumes, pos(0, subvol as u64),
                                                   BtreeIterFlags::CACHED);
    if matches!(&ret, Ok(k) if k.v.state_decoded() == Some(c::bch_subvolume_state::SUBVOLUME_STATE_deleted)) {
        ret = Err(fs.err(bch_errcode::BCH_ERR_ENOENT_subvolume_deleted));
    }
    if let Err(e) = &ret {
        if e.matches(c::ENOENT) && inconsistent_if_not_found {
            missing(fs, subvol)?;
        }
    }
    ret
}

pub fn get(
    t:                         &TransAttempt<'_, '_>,
    subvol:                    u32,
    inconsistent_if_not_found: bool,
) -> Result<c::Subvolume, BchError> {
    Ok(get_key(t, subvol, inconsistent_if_not_found)?.v)
}

#[c_export]
pub fn bch2_subvolume_get(
    trans:                     &Opaque<c::btree_trans>,
    subvol:                    core::ffi::c_uint,
    inconsistent_if_not_found: bool,
    s:                         &mut c::Subvolume,
) -> core::ffi::c_int {
    ret_to_c(get(&unsafe { BtreeTrans::from_c(trans).attempt_in_progress() }, subvol,
                 inconsistent_if_not_found).map(|v| *s = v))
}

/// Unlike get(), finds a tombstone's snapshot.
fn __get_snapshot(t: &TransAttempt<'_, '_>, subvol: u32, warn: bool) -> Result<u32, BchError> {
    let fs = t.fs();
    let mut iter = BtreeIter::new(t, c::btree_id::subvolumes, pos(0, subvol as u64),
                                  BtreeIterFlags::CACHED);
    match iter.peek_slot_typed::<c::Subvolume>(t) {
        Ok(k) => Ok(k.v().snapshot()),
        Err(e) => {
            if warn && e.matches(c::ENOENT) {
                missing(fs, subvol)?;
            }
            Err(e)
        }
    }
}

pub fn get_snapshot(t: &TransAttempt<'_, '_>, subvol: u32) -> Result<u32, BchError> {
    __get_snapshot(t, subvol, true)
}

/// For callers that handle a missing subvolume themselves: not reported.
pub fn get_snapshot_nowarn(t: &TransAttempt<'_, '_>, subvol: u32) -> Result<u32, BchError> {
    __get_snapshot(t, subvol, false)
}

#[c_export]
pub fn __bch2_subvolume_get_snapshot(
    trans:  &Opaque<c::btree_trans>,
    subvol: u32,
    snapid: &mut u32,
    warn:   bool,
) -> core::ffi::c_int {
    ret_to_c(__get_snapshot(&unsafe { BtreeTrans::from_c(trans).attempt_in_progress() }, subvol, warn)
             .map(|s| *snapid = s))
}

#[c_export]
pub fn bch2_subvolume_get_snapshot(
    trans:  &Opaque<c::btree_trans>,
    subvol: u32,
    snapid: &mut u32,
) -> core::ffi::c_int {
    ret_to_c(get_snapshot(&unsafe { BtreeTrans::from_c(trans).attempt_in_progress() }, subvol)
             .map(|s| *snapid = s))
}

/// Unlinked is the only state in which a subvolume's root may be an unlinked
/// directory.
pub fn is_unlinked(t: &TransAttempt<'_, '_>, subvol: u32) -> Result<bool, BchError> {
    match get(t, subvol, false) {
        Ok(s) => Ok(s.state_decoded() == Some(c::bch_subvolume_state::SUBVOLUME_STATE_unlinked)),
        Err(e) if e.matches(c::ENOENT) => Ok(false),
        Err(e) => Err(e),
    }
}

/// EROFS if @subvol is read-only or unlinked, else its snapshot.
pub fn is_ro_trans(t: &TransAttempt<'_, '_>, subvol: u32) -> Result<u32, BchError> {
    let s = get(t, subvol, true)?;

    if s.ro() || s.state_decoded() == Some(c::bch_subvolume_state::SUBVOLUME_STATE_unlinked) {
        return Err(BchError::from(c::EROFS));
    }
    Ok(s.snapshot())
}

#[c_export]
pub fn bch2_subvol_is_ro_trans(
    trans:  &Opaque<c::btree_trans>,
    subvol: u32,
    snapid: &mut u32,
) -> core::ffi::c_int {
    ret_to_c(is_ro_trans(&unsafe { BtreeTrans::from_c(trans).attempt_in_progress() }, subvol)
             .map(|s| *snapid = s))
}

pub fn is_ro(fs: &Fs, subvol: u32) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    lockrestart_do(&trans, |t| is_ro_trans(t, subvol).map(|_| ()))
}

#[c_export]
pub fn bch2_subvol_is_ro(c: &Opaque<c::bch_fs>, subvol: u32) -> core::ffi::c_int {
    ret_to_c(is_ro(&Fs::from_c(c), subvol))
}

/// ENOTEMPTY_subvol_not_empty if @subvol has subvolumes under it.
pub fn require_no_children(t: &TransAttempt<'_, '_>, subvol: u32) -> Result<(), BchError> {
    let mut iter = BtreeIter::new(t, c::btree_id::subvolume_children, pos(subvol as u64, 0),
                                  BtreeIterFlags::empty());
    match iter.peek(t)? {
        Some(k) if k.k.p.inode == subvol as u64 =>
            Err(t.fs().err(bch_errcode::BCH_ERR_ENOTEMPTY_subvol_not_empty)),
        _ => Ok(()),
    }
}

/// Walk @iter to @end at @subvol's snapshot, calling @f on each key until it
/// breaks. A restart re-reads the snapshot and retries the current key, so @f
/// must not restart after doing something it can't redo. @f may unlock: the
/// next peek relocks.
pub fn for_each_in_subvolume_max_in_trans<'t, F, R>(
    trans:  &BtreeTrans<'t>,
    iter:   &mut BtreeIter<'t>,
    end:    c::bpos,
    subvol: u32,
    flags:  BtreeIterFlags,
    mut f:  F,
) -> Result<R::Break, BchError>
where
    F: FnMut(&TransAttempt<'_, 't>, BkeySC<'_>) -> Result<R, BchError>,
    R: LoopControl,
{
    // One attempt for the walk, not one per key - readdir doesn't need a
    // begin per key; a restart begins the next.
    let mut t = trans.begin();
    let mut snapshot = 0;

    loop {
        let ret = (|| -> Result<Option<R::Break>, BchError> {
            t.relock()?;
            if snapshot == 0 {
                snapshot = get_snapshot(&t, subvol)?;
                iter.set_snapshot(snapshot);
            }
            Ok(match iter.peek_max_type(&t, end, flags)? {
                Some(k) => f(&t, k)?.into_break(),
                None    => Some(Default::default()),
            })
        })();

        match ret {
            Ok(Some(b)) => return Ok(b),
            Ok(None)    => {
                t.verify_not_restarted();
                if !iter.advance() {
                    return Ok(Default::default());
                }
            }
            Err(e) if is_restart(&e) => {
                t = trans.begin();
                snapshot = 0;
            }
            Err(e) => return Err(e),
        }
    }
}

// ── The state field ──────────────────────────────────────────────────────

/// Zero padded past the fields an older key lacks.
pub fn val(k: BkeySC<'_>) -> Option<c::Subvolume> {
    c::Subvolume::val_copy_pad(k)
}

impl fmt::Display for c::bch_subvolume_state {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = SUBVOLUME_STATE_VALUES.iter()
            .find(|(_, v)| *v == self.0 as u64)
            .map_or("(invalid state)", |(n, _)| n);
        f.write_str(name)
    }
}

const SUBVOLUME_STATES: [c::bch_subvolume_state; 3] = [
    c::bch_subvolume_state::SUBVOLUME_STATE_live,
    c::bch_subvolume_state::SUBVOLUME_STATE_unlinked,
    c::bch_subvolume_state::SUBVOLUME_STATE_deleted,
];

/// Any subvolume, in place or whole - the struct reaches these too.
impl c::Subvolume<Validated> {
    pub fn snapshot(&self) -> u32 { self.snapshot.get() }

    /// None if the field is 0 - the key predates it - or damaged.
    pub fn state_field(&self) -> Option<c::bch_subvolume_state> {
        SUBVOLUME_STATES.into_iter().find(|&s| s.0 == self.state().get())
    }

    /// The state the legacy UNLINKED_OBSOLETE flag implies.
    pub fn state_from_flags(&self) -> c::bch_subvolume_state {
        if self.unlinked_obsolete() { c::bch_subvolume_state::SUBVOLUME_STATE_unlinked } else { c::bch_subvolume_state::SUBVOLUME_STATE_live }
    }

    /// From the legacy flag if the key predates the state field; None if the
    /// field is damaged.
    pub fn state_decoded(&self) -> Option<c::bch_subvolume_state> {
        if self.state() == 0 { Some(self.state_from_flags()) } else { self.state_field() }
    }
}

/// The struct: a field an older key lacks, to set.
impl c::Subvolume<Whole> {
    /// Also sets the legacy flag for either non-live state: a state field
    /// wiped and recovered from the flag comes back unlinked, so the deletion
    /// reruns rather than being reverted, and an old kernel keeps deleting a
    /// tombstone.
    pub fn set_state(&mut self, state: c::bch_subvolume_state) {
        self.set_unlinked_obsolete(state != c::bch_subvolume_state::SUBVOLUME_STATE_live);
        self.state = state.0.into();
    }
}

/// See snapshot::state_nearest().
fn state_nearest(raw: u32) -> (c::bch_subvolume_state, u32) {
    let mut best = (c::bch_subvolume_state::SUBVOLUME_STATE_live, 33);
    for st in SUBVOLUME_STATES {
        let dist = (raw ^ st.0).count_ones();
        if dist < best.1 {
            best = (st, dist);
        }
    }
    best
}

// ── Validate, to_text, the trigger ───────────────────────────────────────

fn validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
    let k = v.k;
    let s = val(k).expect("a subvolume");

    bkey_fsck_err_on!(v, k.k.p.inode != 0 || k.k.p.offset < 1 || k.k.p.offset > i32::MAX as u64,
                      id::subvol_pos_bad, "invalid pos")?;
    bkey_fsck_err_on!(v, s.snapshot == 0, id::subvol_snapshot_bad, "invalid snapshot")?;
    bkey_fsck_err_on!(v, s.inode == 0, id::subvol_inode_bad, "invalid inode")?;

    if has_field!(k, pad) {
        bkey_fsck_err_on!(v, s.pad != 0, id::subvol_pad_nonzero, "reserved pad field nonzero")?;
    }

    // Only at commit, never on existing keys. Only Ok(false) fails: is_leaf()
    // errors for ids not in the table yet, and create() commits the subvolume
    // with its new snapshot nodes, before their trigger adds them.
    if v.from.from() == c::bkey_validate_from::BKEY_VALIDATE_commit &&
       v.fs.opts().no_commit_validate == 0 {
        if has_field!(k, state) {
            bkey_fsck_err_on!(v, s.state != 0 && s.state_field().is_none(), id::subvol_state_bad,
                              "invalid state {:#x}", s.state.get())?;
        }

        let snapshot = s.snapshot.get();
        bkey_fsck_err_on!(v, matches!(v.fs.snapshots.is_leaf(snapshot), Ok(false)),
                          id::subvol_snapshot_not_leaf,
                          "snapshot {snapshot} is an interior node (subvolumes only reference leaves)")?;
    }
    Ok(())
}

#[c_export]
pub fn bch2_subvolume_validate(
    c:    &Opaque<c::bch_fs>,
    k:    BkeySC<'_>,
    from: &c::bkey_validate_context,
) -> core::ffi::c_int {
    ret_to_c(validate(&BkeyValidate { fs: &Fs::from_c(c), k, from }))
}

#[cold]
#[c_export]
pub fn bch2_subvolume_to_text(
    out: &mut Printbuf,
    _c:  &Opaque<c::bch_fs>,
    k:   BkeySC<'_>,
) {
    let s = val(k).expect("a subvolume");

    write!(out, "root {} snapshot id {}", s.inode.get(), s.snapshot.get());

    if has_field!(k, creation_parent) {
        write!(out, " creation_parent {}", s.creation_parent.get());
        write!(out, " fs_parent {}", s.fs_path_parent.get());
    }

    if s.ro() {
        write!(out, " ro");
    }
    if s.snap() {
        write!(out, " snapshot");
    }

    let state = s.state().get();
    if state == 0 {
        write!(out, " {} (not upgraded, unlinked_obsolete={})",
               s.state_from_flags(), s.unlinked_obsolete() as u32);
    } else if let Some(state) = s.state_field() {
        write!(out, " {state}");

        // The legacy flag disagreeing with the state
        if s.unlinked_obsolete() != (state != c::bch_subvolume_state::SUBVOLUME_STATE_live) {
            write!(out, " unlinked_obsolete={}", s.unlinked_obsolete() as u32);
        }
    } else {
        write!(out, " state {state:#x} invalid (unlinked_obsolete={})", s.unlinked_obsolete() as u32);
    }
}

/// POS_MIN if it isn't nested.
fn children_pos(k: BkeySC<'_>) -> c::bpos {
    match val(k) {
        Some(s) if s.fs_path_parent() != 0 => pos(s.fs_path_parent().get() as u64, k.k.p.offset),
        _                                   => POS_MIN,
    }
}

fn children_mod(t: &TransAttempt<'_, '_>, p: c::bpos, set: bool) -> Result<(), BchError> {
    if p == POS_MIN {
        return Ok(());
    }
    t.bit_mod(c::btree_id::subvolume_children, p, set)
}

fn trigger(t: &TransAttempt<'_, '_>, op: &TriggerOp<'_>) -> Result<(), BchError> {
    if op.flags.contains(UpdateTriggerFlags::TRANSACTIONAL) {
        passes::clear_btree_clean(t.fs(), c::btree_id::subvolumes);

        let old = children_pos(op.old);
        let new = children_pos(op.new.as_sc());

        if old != new {
            children_mod(t, old, false)?;
            children_mod(t, new, true)?;
        }
    }
    Ok(())
}

crate::key_trigger!(bch2_subvolume_trigger => trigger);

// ── fsck ─────────────────────────────────────────────────────────────────

fn check_subvol<'a, 't>(
    t:    &TransAttempt<'a, 't>,
    iter: &BtreeIter<'t>,
    k:    BkeySC<'_>,
) -> Result<(), BchError> {
    let fs = t.fs();

    let Some(mut s) = CowKey::<c::Subvolume>::new(iter, k) else { return Ok(()) };
    let id = s.pos().offset;

    // Predates the state field, or was wiped: silent mid-upgrade, an error
    // after.
    if s.v().state().get() == 0 {
        let upgrading = fs.version_upgrade_complete() <
            c::bcachefs_metadata_version::per_dev_fragmentation_lru;
        if upgrading ||
           fsck_err!(t, id::subvol_state_bad,
                     "subvolume state unset, recovering from legacy flags:\n{}", s.to_text(fs))? {
            let state = s.v().state_from_flags();
            s.v_mut(t)?.state = state.0.into();
        }
    }

    // The repairs below go by the state, so it has to be readable first.
    if s.v().state_field().is_none() {
        let raw = s.v().state().get();
        let (nearest, dist) = state_nearest(raw);

        // Codewords are >= 14 bits apart: within 6 bits, the nearest is the
        // original
        let fix = if dist <= 2 {
            fsck_err!(t, id::subvol_state_bitflip,
                      "subvolume state {raw:#x} is a {dist}-bit flip of {nearest} - correcting:\n{}",
                      s.to_text(fs))?
        } else if dist <= 6 {
            fsck_err!(t, id::subvol_state_bad,
                      "subvolume state {raw:#x} is {dist} bits from {nearest} - correcting:\n{}",
                      s.to_text(fs))?
        } else {
            fsck_err_report!(fs, id::subvol_state_bad,
                             "subvolume has invalid state {raw:#x} (nearest codeword {nearest} is {dist} bits away):\n{}",
                             s.to_text(fs));
            return fs.throw(bch_errcode::BCH_ERR_fsck_repair_unimplemented);
        };

        if fix {
            s.v_mut(t)?.set_state(nearest);
        }
    }

    // A tombstone only waits for its snapshot to be reaped; none of the checks
    // below apply, and its root inode may already be gone.
    if s.v().state_decoded() == Some(c::bch_subvolume_state::SUBVOLUME_STATE_deleted) {
        return Ok(());
    }

    let snapid = s.v().snapshot();
    let mut snap_iter = BtreeIter::new(t, c::btree_id::snapshots, pos(0, snapid as u64),
                                       BtreeIterFlags::INTENT);
    let (snap_iter, sk) = match snap_iter.peek_slot_typed_shared::<c::Snapshot>(t) {
        Ok(r) => r,
        Err(e) if e.matches(c::ENOENT) => {
            let mut msg = Printbuf::new();
            writeln!(msg, "subvolume points to missing snapshot\n{}", s.to_text(fs));
            let ret = passes::run_explicit(fs, &mut msg, BCH_RECOVERY_PASS_reconstruct_snapshots,
                                           c::bch_run_recovery_pass_flags::from_bits_retain(0));
            bch_notice!(fs, "{}", msg);
            ret?;
            return Err(e);
        }
        Err(e) => return Err(e),
    };
    let mut n = CowKey::<c::Snapshot>::new(snap_iter, sk.s_c()).expect("a snapshot");

    // The repairs below assume we own this snapshot. If it backrefs another
    // subvolume that also points at it, that one is the owner - don't let
    // whichever is checked last take it.
    let backref = n.v().subvol();

    if backref != 0 && backref as u64 != id {
        match get(t, backref, false) {
            Ok(other) if other.snapshot() == snapid => {
                fsck_err_report!(fs, id::snapshot_subvol_backref_wrong,
                                 "snapshot {snapid} is claimed by subvolume {backref}, which it backrefs, and also by {id}:\n{}\n{}",
                                 s.to_text(fs), n.v());
                return fs.throw(bch_errcode::BCH_ERR_fsck_repair_unimplemented);
            }
            Err(e) if !e.matches(c::ENOENT) => return Err(e),
            _ => {}
        }
    }

    // Subvolumes only point at leaves. No repair: which descendant to point
    // at isn't decidable here. Before the unlinked branch, since
    // set_deleted() would mark an interior node will_delete.
    if n.v().children()[0] != 0 {
        fsck_err_report!(fs, id::subvol_snapshot_not_leaf,
                         "subvolume points to interior snapshot node:\n{}\n{}",
                         s.to_text(fs), n.v());
        return fs.throw(bch_errcode::BCH_ERR_fsck_repair_unimplemented);
    }

    if s.v().state_decoded() == Some(c::bch_subvolume_state::SUBVOLUME_STATE_unlinked) {
        // Repairs above may still be queued: commit them, and rerun
        // check_subvol() - whose fixes are then in, and don't refire -
        // before deleting. Free when nothing's queued.
        t.commit_lazy(CommitFlags::NO_ENOSPC)?;
        return Err(t.close(CommitFlags::NO_ENOSPC, |trans|
            bch_err_msg!(fs, set_deleted(trans, id as u32), "deleting subvolume {id}")));
    }

    // A live subvolume's snapshot must be live and point back at it. This is
    // the side that can repair it: check_snapshots' state repair needs the
    // backref, and its backref repair needs the state live, so a torn snapshot
    // deletion - will_delete set, backref cleared, no tombstone - gets past
    // both. Backref first: set_state() derives SUBVOL_OBSOLETE from it.
    if fsck_err_on!(t, backref as u64 != id, id::snapshot_subvol_backref_wrong,
                    "subvolume points to a snapshot that doesn't point back:\n{}\n{}",
                    s.to_text(fs), n.v())? {
        n.v_mut(t)?.subvol = (id as u32).into();
    }

    if fsck_err_on!(t, n.v().state_decoded() != Some(c::bch_snapshot_state::SNAPSHOT_STATE_live),
                    id::snapshot_subvol_state_mismatch,
                    "subvolume points to a snapshot that isn't live:\n{}\n{}",
                    s.to_text(fs), n.v())? {
        n.v_mut(t)?.set_state(c::bch_snapshot_state::SNAPSHOT_STATE_live);
    }

    if fsck_err_on!(t, id == c::BCACHEFS_ROOT_SUBVOL as u64 && s.v().fs_path_parent() != 0,
                    id::subvol_root_fs_path_parent_nonzero,
                    "root subvolume has nonzero fs_path_parent\n{}", s.to_text(fs))? {
        s.v_mut(t)?.fs_path_parent = 0.into();
    }

    if s.v().fs_path_parent() != 0 {
        let p = children_pos(s.key());
        let mut children_iter = BtreeIter::new(t, c::btree_id::subvolume_children, p,
                                               BtreeIterFlags::empty());
        let ck = children_iter.peek_slot(t)?.expect("a slot always has a key");

        if fsck_err_on!(t, ck.k.type_ != c::bch_bkey_type::KEY_TYPE_set.0 as u8,
                        id::subvol_children_not_set,
                        "subvolume not set in subvolume_children btree at {}:{}\n{}",
                        { p.inode }, { p.offset }, s.to_text(fs))? {
            t.bit_mod(c::btree_id::subvolume_children, p, true)?;
        }
    }

    let root = c::subvol_inum { subvol: id, inum: s.v().inode.get() };
    match inode::peek(t, &mut BtreeIter::uninit(), root, BtreeIterFlags::empty()) {
        Ok(mut inode) => {
            if fsck_err_on!(t, inode.bi_subvol as u64 != id, id::subvol_root_wrong_bi_subvol,
                            "subvol root {}:{} has wrong bi_subvol field: got {}, should be {id}\n{}\nsnapshot {snapid}: {}\n{}",
                            inode.bi_inum, inode.bi_snapshot, inode.bi_subvol,
                            s.to_text(fs), n.v(), InodeText(&inode))? {
                inode.bi_subvol = id as u32;
                inode.bi_snapshot = s.v().snapshot();
                inode::fsck_write(t, &mut inode)?;
            }
        }
        Err(e) if e.matches(c::ENOENT) => {
            if fsck_err!(t, id::subvol_to_missing_root,
                         "subvolume {id} points to missing subvolume root {}:{}",
                         root.inum, s.v().snapshot())? {
                // Recreated empty; check_directory_structure reattaches its
                // contents under lost+found
                let mut inode = c::bch_inode_unpacked::default();
                inode::init_early(fs, &mut inode);
                inode::init_late(fs, &mut inode, fs.current_time(), 0, 0,
                                 (c::S_IFDIR | 0o700) as c::umode_t, 0, None);
                inode.bi_inum          = root.inum;
                inode.bi_snapshot      = s.v().snapshot();
                inode.bi_subvol        = id as u32;
                inode.bi_parent_subvol = s.v().fs_path_parent().get();
                inode::fsck_write(t, &mut inode)?;
            }
        }
        Err(e) => return Err(e),
    }

    if !s.v().snap() {
        let snapshot_root = fs.snapshots.root(s.v().snapshot());
        let tree = fs.snapshots.tree(snapshot_root);

        let st = match snapshot::tree_lookup(t, tree) {
            Ok(st) => st,
            Err(e) => {
                if e.matches(c::ENOENT) {
                    fs_inconsistent!(fs, "check_subvol: snapshot tree {tree} not found");
                }
                return Err(e);
            }
        };

        if fsck_err_on!(t, st.master_subvol.get() as u64 != id,
                        id::subvol_not_master_and_not_snapshot,
                        "subvolume {id} is not set as snapshot but is not master subvolume")? {
            s.v_mut(t)?.set_snap(true);
        }
    }
    Ok(())
}

struct InodeText<'i>(&'i c::bch_inode_unpacked);

impl fmt::Display for InodeText<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = Printbuf::new();
        inode::unpacked_to_text(&mut out, self.0);
        write!(f, "{out}")
    }
}

fn check_subvols(fs: &Fs) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    let mut iter = BtreeIter::new(&trans, c::btree_id::subvolumes, POS_MIN,
                                  BtreeIterFlags::PREFETCH);
    iter.for_each_commit(&trans, None, CommitFlags::NO_ENOSPC,
                         |t, iter, k| check_subvol(t, iter, k))?;

    // A clean pass lets check_key_has_snapshot trust the in-memory table
    if !fs.flag(c::bch_fs_flags::BCH_FS_error) {
        passes::set_btree_clean(fs, c::btree_id::subvolumes);
    }
    Ok(())
}

crate::recovery_pass!(bch2_check_subvols => check_subvols);

fn check_subvol_child(
    t:    &TransAttempt<'_, '_>,
    iter: &BtreeIter<'_>,
    k:    BkeySC<'_>,
) -> Result<(), BchError> {
    let mut s_iter = BtreeIter::new(t, c::btree_id::subvolumes, pos(0, k.k.p.offset),
                                    BtreeIterFlags::empty());
    let s = val(s_iter.peek_slot(t)?.expect("a slot always has a key"));

    // A missing subvolume means a stray entry, not a failed pass
    if fsck_err_on!(t, s.is_none_or(|s| s.state_decoded() != Some(c::bch_subvolume_state::SUBVOLUME_STATE_live) ||
                                        s.fs_path_parent().get() as u64 != k.k.p.inode),
                    id::subvol_children_bad,
                    "incorrect entry in subvolume_children btree {}:{}",
                    { k.k.p.inode }, { k.k.p.offset })? {
        t.delete_at(iter, UpdateTriggerFlags::empty())?;
    }
    Ok(())
}

fn check_subvol_children(fs: &Fs) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    let mut iter = BtreeIter::new(&trans, c::btree_id::subvolume_children, POS_MIN,
                                  BtreeIterFlags::PREFETCH);
    iter.for_each_commit(&trans, None, CommitFlags::NO_ENOSPC,
                         |t, iter, k| check_subvol_child(t, iter, k))
}

crate::recovery_pass!(bch2_check_subvol_children => check_subvol_children);

// ── Unlinking, deleting ──────────────────────────────────────────────────

/// Already queued as an update.
fn get_mut<'a, 't>(t: &TransAttempt<'a, 't>, subvol: u32)
    -> Result<crate::btree::iter::TransBkey<'a, 't, c::Subvolume>, BchError>
{
    let ret = t.bkey_get_mut::<c::Subvolume>(c::btree_id::subvolumes, pos(0, subvol as u64),
                                             BtreeIterFlags::CACHED, UpdateTriggerFlags::empty());
    if let Err(e) = &ret {
        if e.matches(c::ENOENT) {
            missing(t.fs(), subvol)?;
        }
    }
    ret
}

/// For unlinking and deleting: also takes it out of the filesystem tree.
fn set_state_trans(t: &TransAttempt<'_, '_>, subvol: u32, state: c::bch_subvolume_state)
    -> Result<(), BchError>
{
    let mut k = get_mut(t, subvol)?;
    let s = k.v_mut();
    s.set_state(state);
    s.fs_path_parent = 0.into();
    Ok(())
}

/// @old is being deleted: subvolumes created from it name @new instead.
fn reparent(
    t:    &TransAttempt<'_, '_>,
    iter: &BtreeIter<'_>,
    k:    BkeySC<'_>,
    old:  u32,
    new:  u32,
) -> Result<(), BchError> {
    let Some(k) = k.arm::<c::Subvolume>() else { return Ok(()) };
    let s = k.v();

    // A key without creation_parent predates the field: nobody's child
    if !has_field!(k, creation_parent) || s.creation_parent().get() != old {
        return Ok(());
    }

    let mut u = t.bkey_make_mut(iter, k, UpdateTriggerFlags::empty())?;
    u.v_mut().creation_parent = new.into();
    Ok(())
}

/// Point @subvol's snapshots at @subvol's own creation_parent.
fn reparent_children(trans: &BtreeTrans<'_>, subvol: u32) -> Result<(), BchError> {
    let s = lockrestart_do(trans, |t| get(t, subvol, true))?;
    let new = s.creation_parent.get();

    let mut iter = BtreeIter::new(trans, c::btree_id::subvolumes, POS_MIN,
                                  BtreeIterFlags::PREFETCH);
    iter.for_each_commit(trans, None, CommitFlags::NO_ENOSPC,
                         |t, iter, k| reparent(t, iter, k, subvol, new))
}

/// Make @subvol a tombstone, clear it as its tree's master, and mark its
/// snapshot will_delete.
fn __set_deleted(t: &TransAttempt<'_, '_>, subvol: u32) -> Result<(), BchError> {
    let fs = t.fs();

    let mut iter = BtreeIter::new(t, c::btree_id::subvolumes, pos(0, subvol as u64),
                                  BtreeIterFlags::CACHED | BtreeIterFlags::INTENT);
    let k = match iter.peek_slot_typed::<c::Subvolume>(t) {
        Err(e) if e.matches(c::ENOENT) => {
            missing(fs, subvol)?;
            return Err(e);
        }
        r => r?,
    };
    let snapid = k.v().snapshot();

    let mut snapshot_iter = BtreeIter::new(t, c::btree_id::snapshots, pos(0, snapid as u64),
                                           BtreeIterFlags::empty());
    let sk = snapshot_iter.peek_slot_typed::<c::Snapshot>(t)
        .inspect_err(|e| if e.matches(c::ENOENT) {
            fs_inconsistent!(fs, "missing snapshot {snapid}");
        })?;
    let tree = sk.v().tree();

    let mut tree_iter = BtreeIter::new(t, c::btree_id::snapshot_trees, pos(0, tree as u64),
                                       BtreeIterFlags::empty());
    let (tree_iter, tk) = tree_iter
        .peek_slot_typed_shared::<c::SnapshotTree>(t)
        .inspect_err(|e| if e.matches(c::ENOENT) {
            fs_inconsistent!(fs, "missing snapshot tree {tree}");
        })?;

    if tk.v().master_subvol.get() == subvol {
        let mut u = t.bkey_make_mut(tree_iter, tk, UpdateTriggerFlags::empty())?;
        u.v_mut().master_subvol = 0.into();
    }

    set_state_trans(t, subvol, c::bch_subvolume_state::SUBVOLUME_STATE_deleted)?;
    snapshot::node_set_deleted(t, snapid)
}

fn set_deleted(trans: &BtreeTrans<'_>, subvol: u32) -> Result<(), BchError> {
    reparent_children(trans, subvol)?;
    commit_do(trans, None, CommitFlags::NO_ENOSPC, |t| __set_deleted(t, subvol))
}

impl c::bch_fs_snapshots {
    fn delete_work(&self) -> &Opaque<c::work_struct> {
        unsafe { &*(&raw const self.wait_for_pagecache_and_delete_work).cast() }
    }
}

/// Waits for inodes still in use.
#[cfg(kernel)]
fn evict_inodes(fs: &Fs, subvols: &[u32]) {
    // Borrowed: C only reads it
    let mut list = c::snapshot_id_list {
        data: subvols.as_ptr() as *mut u32,
        nr:   subvols.len(),
        size: subvols.len(),
        ..Default::default()
    };
    unsafe { c::bch2_evict_subvolume_inodes(fs.raw, &mut list) }
}

#[cfg(not(kernel))]
fn evict_inodes(_fs: &Fs, _subvols: &[u32]) {}

/// Holds the snapshot_delete_pagecache write ref unlink() took.
///
/// # Safety
/// @work is c->snapshots.wait_for_pagecache_and_delete_work, queued through
/// WriteRef::queue_work().
#[c_export]
pub unsafe fn bch2_subvolume_wait_for_pagecache_and_delete(work: *mut c::work_struct) {
    let c = unsafe {
        work.byte_sub(offset_of!(c::bch_fs, snapshots.wait_for_pagecache_and_delete_work))
            .cast::<c::bch_fs>()
    };
    let fs = unsafe { Fs::borrow_raw(c) };
    let _write_ref = unsafe {
        WriteRef::adopt(&fs, c::bch_write_ref::BCH_WRITE_REF_snapshot_delete_pagecache)
    };

    loop {
        let subvols = core::mem::replace(&mut *fs.snapshots.unlinked.lock(), KVVec::new());
        if subvols.is_empty() {
            break;
        }

        evict_inodes(&fs, &subvols);

        let trans = crate::btree_trans!(&fs);
        for &id in subvols.iter() {
            if bch_err_msg!(&fs, set_deleted(&trans, id), "deleting subvolume {id}").is_err() {
                return;
            }
        }
    }
}

/// On commit, queues the delete work.
pub fn unlink(t: &TransAttempt<'_, '_>, subvol: u32) -> Result<(), BchError> {
    t.commit_hook(move |trans| {
        let fs = trans.fs();

        {
            let mut unlinked = fs.snapshots.unlinked.lock();
            if !unlinked.contains(&subvol) {
                unlinked.push(subvol, crate::util::alloc::flags::GFP_KERNEL)?;
            }
        }

        let Some(write_ref) =
            fs.write_ref_tryget(c::bch_write_ref::BCH_WRITE_REF_snapshot_delete_pagecache) else {
            return Err(BchError::from(c::EROFS));
        };
        write_ref.queue_work(fs.snapshots.delete_work());
        Ok(())
    })?;

    set_state_trans(t, subvol, c::bch_subvolume_state::SUBVOLUME_STATE_unlinked)?;

    // There's no unlinked-subvolumes btree, as there is for inodes: if we
    // crash while it's still open, check_subvols finds and deletes it
    passes::set_no_ratelimit(t.fs(), BCH_RECOVERY_PASS_check_subvols);
    Ok(())
}

// ── Creating ─────────────────────────────────────────────────────────────

pub struct Created {
    pub subvol:   u32,
    pub snapshot: u32,
    pub v:        c::Subvolume,
}

/// A subvolume rooted at @inode, under @parent in the filesystem tree; a
/// snapshot of @src, or empty if @src is 0.
pub fn create(
    t:      &TransAttempt<'_, '_>,
    inode:  u64,
    parent: u32,
    src:    u32,
    ro:     bool,
) -> Result<Created, BchError> {
    let fs = t.fs();

    let mut dst_iter = BtreeIter::uninit();
    match t.bkey_get_empty_slot(&mut dst_iter, c::btree_id::subvolumes, POS_MIN,
                                pos(0, u32::MAX as u64)) {
        Err(e) if e.matches(bch_errcode::BCH_ERR_ENOSPC_btree_slot) =>
            return fs.throw(bch_errcode::BCH_ERR_ENOSPC_subvolume_create),
        r => r?,
    }
    let new_id = dst_iter.pos().offset as u32;

    let mut new_nodes = [0u32; 2];
    let snapshot_subvols = [new_id, src];

    // Creating a snapshot:
    let mut src_k = None;
    let mut parent_snapshot = 0;
    if src != 0 {
        let k = get_mut(t, src)?;
        parent_snapshot = k.v().snapshot.get();
        src_k = Some(k);
    }

    let nr = if src != 0 { 2 } else { 1 };
    t.snapshot_node_create(parent_snapshot, &mut new_nodes[..nr], &snapshot_subvols[..nr])?;

    if let Some(mut k) = src_k {
        k.v_mut().snapshot = new_nodes[1].into();
    }

    let mut k = t.bkey_alloc_typed::<c::Subvolume>()?;
    k.k_mut().p = dst_iter.pos();
    let v = k.v_mut();
    v.flags           = 0.into();
    v.snapshot        = new_nodes[0].into();
    v.inode           = inode.into();
    v.creation_parent = src.into();
    v.fs_path_parent  = parent.into();
    v.otime.lo        = fs.current_time().into();
    v.otime.hi        = 0.into();
    v.set_ro(ro);
    v.set_snap(src != 0);
    v.set_state(c::bch_subvolume_state::SUBVOLUME_STATE_live);
    let v = *k.v();

    t.update(&dst_iter, &k, UpdateTriggerFlags::empty())?;

    Ok(Created { subvol: new_id, snapshot: new_nodes[0], v })
}

// ── A new filesystem, an upgrade ─────────────────────────────────────────

/// The root snapshot tree, snapshot and subvolume of a new filesystem.
fn initialize(fs: &Fs) -> Result<(), BchError> {
    let mut root_tree = BkeyI::<c::SnapshotTree>::new();
    root_tree.k.p.offset = 1;
    let v = &mut root_tree.v;
    v.master_subvol = (1u32).into();
    v.root_snapshot = (u32::MAX).into();

    let mut root_snapshot = BkeyI::<c::Snapshot>::new();
    root_snapshot.k.p.offset = u32::MAX as u64;
    let v = &mut root_snapshot.v;
    v.flags  = 0.into();
    v.parent = 0.into();
    v.subvol = (c::BCACHEFS_ROOT_SUBVOL).into();
    v.tree   = (1u32).into();
    v.set_state(c::bch_snapshot_state::SNAPSHOT_STATE_live);

    let mut root_volume = BkeyI::<c::Subvolume>::new();
    root_volume.k.p.offset = c::BCACHEFS_ROOT_SUBVOL as u64;
    let v = &mut root_volume.v;
    v.flags    = 0.into();
    v.snapshot = (u32::MAX).into();
    v.inode    = ((c::BCACHEFS_ROOT_INO as u64)).into();
    v.set_state(c::bch_subvolume_state::SUBVOLUME_STATE_live);

    fs.btree_insert(c::btree_id::snapshot_trees, &mut root_tree, None, CommitFlags::empty(),
                    BtreeIterFlags::empty())?;
    fs.btree_insert(c::btree_id::snapshots, &mut root_snapshot, None, CommitFlags::empty(),
                    BtreeIterFlags::empty())?;
    fs.btree_insert(c::btree_id::subvolumes, &mut root_volume, None, CommitFlags::empty(),
                    BtreeIterFlags::empty())
}

#[c_export]
pub fn bch2_initialize_subvolumes(c: &Opaque<c::bch_fs>) -> core::ffi::c_int {
    ret_to_c(initialize(&Fs::from_c(c)))
}

/// Set bi_subvol on the root inode, upgrading to subvolumes.
fn fs_upgrade_for_subvolumes(fs: &Fs) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    commit_do(&trans, None, CommitFlags::NO_ENOSPC, |t| {
        let mut iter = BtreeIter::new(t, c::btree_id::inodes,
                                      c::bpos { inode: 0, offset: c::BCACHEFS_ROOT_INO as u64,
                                                snapshot: u32::MAX },
                                      BtreeIterFlags::empty());
        let k = iter.peek_slot(t)?.expect("a slot always has a key");

        if !inode::bkey_is_inode(k.k) {
            bch_err!(fs, "root inode not found");
            return fs.throw(bch_errcode::BCH_ERR_ENOENT_inode);
        }

        let mut inode = inode::unpack(fs, k);
        inode.bi_subvol = c::BCACHEFS_ROOT_SUBVOL;
        inode::write(t, &mut iter, &mut inode)
    })
}

crate::recovery_pass!(bch2_fs_upgrade_for_subvolumes => fs_upgrade_for_subvolumes);

/// # Safety
/// Once, from bch2_fs_init(), on a zeroed bch_fs before any use.
#[c_export]
pub unsafe fn bch2_fs_subvolumes_init_early(c: &Opaque<c::bch_fs>) {
    let raw = Fs::from_c(c).raw;
    unsafe { RustOnly::init(&raw mut (*raw).snapshots.unlinked, new_mutex!(KVVec::new())) }
}

/// # Safety
/// Once, freeing bch_fs: init_early ran (nothing before it in bch2_fs_init()
/// can fail), and nothing uses it after.
#[c_export]
pub unsafe fn bch2_fs_subvolumes_exit(c: &Opaque<c::bch_fs>) {
    let raw = Fs::from_c(c).raw;
    unsafe { RustOnly::drop_in_place(&raw mut (*raw).snapshots.unlinked) }
}
