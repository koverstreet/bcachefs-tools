// SPDX-License-Identifier: GPL-2.0

//! Snapshot deletion.
//!
//! Snapshot trees:
//!
//! A node in a snapshot tree references keys with that snapshot ID, and all
//! keys with ancestor snapshot IDs not overwritten by a descendent snapshot.
//!
//! When a subvolume is deleted, we now have dead and redundant snapshot nodes
//! that must be cleaned up.
//!
//! - Dead:
//!
//!   A snapshot node with no children, and without a subvolume pointing to it,
//!   is unreferenced and can be deleted
//!
//! - Redundant:
//!
//!   Interior snapshot nodes (nodes with children) are only referenced by their
//!   child snapshot nodes. An interior node with only one child is redundant;
//!   we can clean it up by moving all non-overwritten keys to the child
//!   snapshot and removing it from the snapshot tree.
//!
//! Snapshot node states:
//!
//! - WILL_DELETE: this doesn't need to be a separate state bit. Indicates a
//!   leaf node that's no longer referenced by a subvolume
//!   (bch_snapshot.subvol == 0), so it's pending deletion
//!
//! - NO_KEYS: We can't remove interior nodes from the snapshot tree at
//!   runtime, because that can require changing bch_snapshot.depth on
//!   arbitrarily many children, and we can't do that atomically.
//!
//!   So instead, at runtime we'll do the heavy lifting of removing all keys
//!   that reference that snapshot ID, leave it in a half dead state, and the
//!   next time we start up we'll remove it from the snapshot tree.
//!
//!   Technically, we could, because the codepaths where this matters use the
//!   RCU-protected snapshot table - but there's a lot of work that has to be
//!   done for deleting interior snapshot nodes; parent/child pointers need to
//!   be updated, skiplists need to be adjusted, and if we get any of this
//!   wrong things can and will go horrifically wrong.
//!
//!   But if we defer it until recovery, when we're not yet running
//!   multithreaded, we can also run the check_snapshots recovery pass
//!   afterwards, for extra safety.
//!
//! The state - c->snapshots.delete, SnapshotDelete - is Rust's alone. A run
//! is serialized by recovery.run_lock (the pass framework holds it, sysfs's
//! force-trigger takes it), and collects its nodes into locals; DeleteNodes,
//! what sysfs shows, it publishes once collected - an RwSem written there
//! and when the run ends, read by the run and sysfs in between, so neither
//! ever waits on the other. Its no_keys list and the eytzinger lookup list
//! the sweep uses for every key are the run's only, never shared. The
//! progress indicator sits outside the RwSem: it's its own SeqLock, the run
//! updating it as it sweeps.

use crate::accounting;
use crate::alloc::buckets::DiskReservation;
use crate::btree::bkey::{pos, spos, BkeyI, BkeySC, BkeySCTyped, POS_MIN};
use crate::btree::iter::{
    commit_do, lockrestart_do, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, TransAttempt,
    TransBkey, UpdateTriggerFlags,
};
use crate::c;
use crate::c::bch_recovery_pass::*;
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::fs::Fs;
use crate::init::damage;
use crate::init::error::id;
use crate::init::passes;
use crate::snapshots::check_snapshots;
use crate::snapshots::snapshot;
use crate::util::alloc::{flags::GFP_KERNEL, KVVec};
use crate::util::eytzinger::Eytzinger1;
use crate::util::ffi::Opaque;
use crate::util::log::{self, LogLevel, LogMsg};
use crate::util::sync::{new_rwsem, RwSem};
use crate::util::Printbuf;
use crate::{bch_err, bch_err_msg, bch_info, bch_notice, fs_inconsistent, fsck_err, trans_inconsistent};
use core::sync::atomic::{AtomicU32, Ordering};
use cstruct_macros::c_export;
use pin_init::{pin_data, pin_init, PinInit};

crate::module_param! {
    /// Stop the key sweep after the extents/dirents/xattrs/damage pass and
    /// before the inodes are deleted, so a test can construct the state an
    /// interrupted deletion actually leaves: content gone for the dying
    /// snapshots, the inodes that indexed it still there, nodes still live.
    ///
    /// That boundary and not another one: the inodes btree is the index the
    /// sweep scans through, so it is the last point at which anything the
    /// sweep missed is still findable.
    pub(crate) static snapshot_delete_bail_before_inodes: bool = false;
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    perm = 0o644, desc = "Cancel snapshot deletion before the inode keys are deleted";
}

// ── State ────────────────────────────────────────────────────────────────

/// A node a deletion empties: an interior node, and the live child its keys
/// migrate to - or, in the eytzinger list, a leaf, with no live child.
#[derive(Clone, Copy, Default)]
struct InteriorDelete {
    id:         u32,
    live_child: u32,
}

/// The live child recorded for @id in @l; 0 if it isn't there.
fn interior_delete_has_id(l: &[InteriorDelete], id: u32) -> u32 {
    l.iter().find(|i| i.id == id).map_or(0, |i| i.live_child)
}

/// What a deletion run deletes: what sysfs shows while it runs.
#[derive(Default)]
struct DeleteNodes {
    deleting_from_trees: KVVec<u32>,
    delete_leaves:       KVVec<u32>,
    delete_interior:     KVVec<InteriorDelete>,
}

/// c->snapshots.delete.
#[pin_data]
pub(crate) struct SnapshotDelete {
    /// The running deletion's nodes; None when none is running.
    #[pin]
    nodes:    RwSem<Option<DeleteNodes>>,
    /// Which sweep the progress describes: 1 or 2.
    version:  AtomicU32,
    progress: c::progress_indicator,
}

impl SnapshotDelete {
    pub(crate) fn init() -> impl PinInit<Self> {
        pin_init!(Self {
            nodes    <- new_rwsem!(None),
            version:  AtomicU32::new(0),
            progress: ::pin_init::Zeroable::zeroed(),
        })
    }

    fn start_sweep(&self, fs: &Fs, msg: &'static core::ffi::CStr, mask: u64, version: u32) {
        self.progress.init_mask(fs, msg, mask, 0);
        self.progress.set_silent();
        self.version.store(version, Ordering::Relaxed);
    }
}

/// A run's nodes, published, and what's its alone.
struct Run<'r> {
    nodes:   &'r DeleteNodes,
    no_keys: &'r [InteriorDelete],
    /// Every node being emptied, interior and leaf, in Eytzinger1 order -
    /// slot 0 unused: what the sweep looks each key's snapshot up in.
    dying:   &'r [InteriorDelete],
}

// ── Status ───────────────────────────────────────────────────────────────

#[cold]
fn nodes_to_text(out: &mut Printbuf, nodes: Option<&DeleteNodes>, full: bool) {
    let limit = if full { usize::MAX } else { 10 };
    let empty = DeleteNodes::default();
    let d = nodes.unwrap_or(&empty);

    let _ = write!(out, "deleting from trees");
    for i in d.deleting_from_trees.iter().take(limit) {
        let _ = write!(out, " {i}");
    }
    if d.deleting_from_trees.len() > limit {
        let _ = write!(out, " (many)");
    }
    let _ = writeln!(out);

    let _ = write!(out, "deleting leaves");
    for i in d.delete_leaves.iter().take(limit) {
        let _ = write!(out, " {i}");
    }
    if d.delete_leaves.len() > limit {
        let _ = write!(out, " (many)");
    }
    let _ = writeln!(out);

    let _ = write!(out, "interior");
    for i in d.delete_interior.iter().take(limit) {
        let _ = write!(out, " {}->{}", i.id, i.live_child);
    }
    if d.delete_interior.len() > limit {
        let _ = write!(out, " (many)");
    }
    let _ = writeln!(out);
}

/// Snapshot deletion's status, for sysfs.
#[c_export]
#[cold]
pub fn bch2_snapshot_delete_status_to_text(out: &mut c::printbuf, c: &Opaque<c::bch_fs>) {
    let fs = Fs::from_c(c);
    let d = &fs.snapshots.delete;
    let out = unsafe { Printbuf::borrow_raw(out) };

    let nodes = d.nodes.read();
    let Some(nodes) = nodes.as_ref() else {
        let _ = write!(out, "(not running)");
        return;
    };

    let _ = writeln!(out, "Snapshot deletion v{}", d.version.load(Ordering::Relaxed));
    let _ = writeln!(out, "Progress: {}", d.progress);
    nodes_to_text(out, Some(nodes), false);
}

// ── Node state ───────────────────────────────────────────────────────────

/// Snapshot node @id's key, as a mutable copy queued for the commit - a
/// missing one is an inconsistency.
fn get_mut_node<'a, 't>(t: &TransAttempt<'a, 't>, id: u32)
    -> Result<TransBkey<'a, 't, c::Snapshot>, BchError>
{
    t.bkey_get_mut(c::btree_id::snapshots, pos(0, id as u64), BtreeIterFlags::empty(),
                   UpdateTriggerFlags::empty())
        .inspect_err(|e| if e.matches(c::ENOENT) {
            fs_inconsistent!(t.fs(), "missing snapshot {id}");
        })
}

/// Snapshot node @id's key - a missing one is an inconsistency.
fn lookup_node(t: &TransAttempt<'_, '_>, id: u32) -> Result<BkeyI<c::Snapshot>, BchError> {
    snapshot::lookup_key(t, id)
        .inspect_err(|e| if e.matches(c::ENOENT) {
            fs_inconsistent!(t.fs(), "missing snapshot {id}");
        })
}

/// The raw state field. A key that predates it reads 0, which matches no
/// state: == deleted is false for a deleted legacy node, and != live true for
/// a live one. For what to do with a node, state_compat().
fn state(v: &c::Snapshot) -> c::bch_snapshot_state {
    c::bch_snapshot_state(v.state().get())
}

/// The node's state, on any key: a zero field means the key predates it, and
/// the old flag bits are authoritative.
fn state_compat(v: &c::Snapshot) -> c::bch_snapshot_state {
    if v.state().get() != 0 { state(v) } else { v.state_from_flags() }
}

/// Mark snapshot @id will_delete, for future cleanup - its subvolume being
/// deleted - if it's live.
pub fn node_set_deleted(t: &TransAttempt<'_, '_>, id: u32) -> Result<(), BchError> {
    let mut s = get_mut_node(t, id)?;

    /* already deleted? */
    if state(s.v()) != c::bch_snapshot_state::SNAPSHOT_STATE_live {
        return Ok(());
    }

    // The backref is retained: it now points at the subvolume's tombstone,
    // and deletion checks it - a will_delete leaf without a subvolume
    // pointing back is an invalid state (check_should_delete_leaf):
    s.v_mut().set_state(c::bch_snapshot_state::SNAPSHOT_STATE_will_delete);
    Ok(())
}

/// Keys and sectors accounted to a snapshot, and the btrees holding them.
pub(crate) struct AccountingTotals {
    pub(crate) keys:    u64,
    pub(crate) sectors: u64,
    /// A bit per btree with anything accounted
    pub(crate) btrees:  u64,
}

/// Total keys/sectors accounted to snapshot @id across the snapshotted
/// btrees, with a per-btree breakdown appended to @breakdown. (nr_keys
/// counters exist only post-upgrade and read as zero before.)
///
/// Sanity checks before destructive snapshot-node transitions (emptying or
/// deleting a node) read this: the per-snapshot disk accounting counters must
/// be zero. The deletion scan should already have migrated or removed every
/// key stamped with this snapshot id; this verifies it did. A nonzero count
/// means a key is still accounted to the node, and one of two things is
/// wrong:
///
///  - the accounting is stale/incorrect, or
///  - the inodes btree is missing an entry: the deletion scan relies on "an
///    extent/dirent/xattr in snapshot X implies an inode in snapshot X" to
///    find the keys to remove, so a missing inode strands that snapshot's
///    keys.
///
/// The key count catches metadata-only stranding (dirents, xattrs, empty
/// inodes) that the sectors counter can't see. It's only trusted once
/// check_allocations has rebuilt it (scheduled by the
/// per_dev_fragmentation_lru upgrade); before that version we fall back to
/// the sectors-only check. Either way it's an in-memory read per snapshot
/// btree, and the per-btree breakdown points at where any stranded keys live.
pub(crate) fn accounting_totals(fs: &Fs, id: u32, mut breakdown: Option<&mut Printbuf>)
    -> AccountingTotals
{
    let trust_keys = fs.version_upgrade_complete().0 >=
        c::bcachefs_metadata_version::bcachefs_metadata_version_per_dev_fragmentation_lru.0;

    let mut ret = AccountingTotals { keys: 0, sectors: 0, btrees: 0 };

    for &btree in crate::BTREE_IDS_KNOWN {
        if crate::BTREE_HAS_SNAPSHOTS_MASK & (1u64 << btree.0) == 0 {
            continue;
        }

        // In-mem: bch2_accounting_is_mem() covers everything but
        // BCH_DISK_ACCOUNTING_inum, so the counters are current as deltas are
        // applied. No btree read, no transaction, and no write buffer flush
        // to make the values trustworthy.
        let mut v = [0u64; 3];
        accounting::mem_read(fs, &c::disk_accounting_pos::from_arm(
                c::bch_acct_snapshot { id, btree: btree.0 }), &mut v);

        let nr_keys   = if trust_keys { v[0] } else { 0 };
        let key_bytes = if trust_keys { v[1] } else { 0 };
        let sectors   = v[2];

        if nr_keys == 0 && sectors == 0 {
            continue;
        }

        ret.keys    += nr_keys;
        ret.sectors += sectors;
        ret.btrees  |= 1u64 << btree.0;

        if let Some(b) = breakdown.as_deref_mut() {
            let _ = write!(b, "\n  {btree}: {nr_keys} keys ({key_bytes} bytes), {sectors} sectors");
        }
    }
    ret
}

/// Keys stranded in @btrees: schedule the content pass for each, which is what
/// repairs the cause (a key with no inode at its own snapshot) and so lets
/// the next deletion find them.
fn schedule_content_passes(fs: &Fs, msg: &mut Printbuf, btrees: u64) -> Result<(), BchError> {
    let mut ret = Ok(());

    for (btree, pass) in [(c::btree_id::extents, BCH_RECOVERY_PASS_check_extents),
                          (c::btree_id::inodes,  BCH_RECOVERY_PASS_check_inodes),
                          (c::btree_id::dirents, BCH_RECOVERY_PASS_check_dirents),
                          (c::btree_id::xattrs,  BCH_RECOVERY_PASS_check_xattrs)] {
        if btrees & (1u64 << btree.0) != 0 {
            let r = passes::run_explicit(fs, msg, pass, c::bch_run_recovery_pass_flags::from_bits_retain(0));
            if r.is_err() {
                ret = r;
            }
        }
    }
    ret
}

/// After a pass's error that doesn't stop the refusal: at runtime the passes
/// can't rewind - but the requirement was persisted to the superblock before
/// cannot_rewind_recovery was returned, so they run at next mount and the
/// refusal is the complete runtime response.
fn rewind_is_fine(ret: Result<(), BchError>) -> Result<(), BchError> {
    match ret {
        Err(e) if e.matches(bch_errcode::BCH_ERR_cannot_rewind_recovery) => Ok(()),
        r => r,
    }
}

/// Refuse - with a repair scheduled - to empty or delete node @id while it
/// still has data accounted to it. @op is the operation delete_dead_snapshots
/// was performing - set_no_keys, leaf delete, or interior delete - which the
/// node key does not tell you, and which is the first thing you need: whether
/// we were emptying a node whose keys should already have migrated down, or
/// splicing out an interior.
///
/// The node is printed too, because operation and shape are independent and
/// their disagreement is the signal: an interior delete refused on a node
/// that still reads as a leaf means something quite different from a leaf
/// delete refused on a redundant interior whose migration was interrupted
/// partway.
///
/// Refuse the transition and schedule check_allocations (recompute
/// accounting) and check_inodes (revalidate the inode<->snapshot mapping) to
/// resolve which, rather than dropping the keys.
fn check_no_data(t: &TransAttempt<'_, '_>, id: u32, op: &str, run: Option<&Run<'_>>)
    -> Result<(), BchError>
{
    let fs = t.fs();

    let mut buf = Printbuf::new();
    let totals = accounting_totals(fs, id, Some(&mut buf));

    if totals.keys == 0 && totals.sectors == 0 {
        return Ok(());
    }

    /* Only the refusal below needs the node itself: */
    let s = lookup_node(t, id)?;

    // Whether an interior has a single live child is what the deletion
    // recorded when it built its lists, not something to re-derive from the
    // snapshot table: live_child is the migration target the deletion
    // actually used, so if the two ever disagreed the list is the one that
    // describes what happened.
    let live_child = run.map_or(0, |r| {
        match interior_delete_has_id(&r.nodes.delete_interior, id) {
            0  => interior_delete_has_id(r.no_keys, id),
            lc => lc,
        }
    });

    let shape = if s.v().children()[0] == 0 {
        "leaf"
    } else if live_child != 0 {
        "redundant interior"
    } else {
        "interior"
    };

    let mut msg = Printbuf::new();
    let _ = writeln!(msg, "{op} snapshot node {id} ({shape}) still has {} keys / {} sectors accounted to it - refusing, to prevent data loss; scheduling repair:{}",
                     totals.keys, totals.sectors, buf.as_str());
    let _ = writeln!(msg, "  {}", BkeySC::from(s.k_i()).to_text(fs));

    // Schedule the passes whose key_has_snapshot repairs can actually remove
    // the stranded keys - per btree, from the accounting breakdown.
    // check_inodes alone can't repair a stranded dirent; scheduling only it
    // left the refusal firing forever. Plus check_allocations, since here the
    // count itself may be what's wrong:
    let mut ret = passes::run_explicit(fs, &mut msg, BCH_RECOVERY_PASS_check_allocations,
                                       c::bch_run_recovery_pass_flags::from_bits_retain(0));

    let r = schedule_content_passes(fs, &mut msg, totals.btrees);
    if r.is_err() {
        ret = r;
    }

    bch_err!(fs, "{}", msg.as_str());

    rewind_is_fine(ret)?;
    fs.throw(bch_errcode::BCH_ERR_EINVAL_snapshot_delete_with_data)
}

/// One node's data, for the log: what each node holds going in - the same
/// counters the check_no_data licenses read on the way out, so whatever is
/// still there at the end is what the migration didn't move.
fn node_data_to_text(out: &mut Printbuf, fs: &Fs, id: u32, live_child: u32) {
    let mut breakdown = Printbuf::new();
    let totals = accounting_totals(fs, id, Some(&mut breakdown));

    let _ = write!(out, "\n  {} {id}", if live_child != 0 { "interior" } else { "leaf" });
    if live_child != 0 {
        let _ = write!(out, " -> {live_child}");
    }
    let _ = write!(out, ": {} keys, {} sectors{}", totals.keys, totals.sectors, breakdown.as_str());
}

/// Every node we're about to delete, and what it holds.
fn delete_data_to_text(fs: &Fs, nodes: &DeleteNodes) -> Printbuf {
    let mut out = Printbuf::new();
    let _ = write!(out, "snapshot deletion, data accounted per node:");

    for &i in nodes.delete_leaves.iter() {
        node_data_to_text(&mut out, fs, i, 0);
    }
    for i in nodes.delete_interior.iter() {
        node_data_to_text(&mut out, fs, i.id, i.live_child);
    }
    out
}

/// Inodes excluded: those are what we're about to delete, accounted until we
/// do.
///
/// Pre-per_dev_fragmentation_lru the key counters aren't populated yet, so
/// this sees sectors only - a dirents- or xattrs-only stranding is invisible
/// there.
fn snapshot_content_empty(fs: &Fs, id: u32, msg: &mut Printbuf, bad_btrees: &mut u64) {
    let mut breakdown = Printbuf::new();
    let totals = accounting_totals(fs, id, Some(&mut breakdown));

    let btrees = totals.btrees & !(1u64 << c::btree_id::inodes.0);
    if btrees == 0 {
        return;
    }

    if *bad_btrees == 0 {
        let _ = write!(msg, "content still accounted to dying snapshots - refusing to \
                             delete their inode keys, which is how the deletion scan finds \
                             these keys at all:");
    }

    *bad_btrees |= btrees;
    let _ = write!(msg, "\n  snapshot {id}:{}", breakdown.as_str());
}

/// Which content btrees still have keys accounted to a dying snapshot, as a
/// mask. Nothing may remain in them by the time we delete the inode keys -
/// see the caller.
fn dying_snapshots_content_btrees(fs: &Fs, nodes: &DeleteNodes, msg: &mut Printbuf) -> u64 {
    let mut bad_btrees = 0;

    for &i in nodes.delete_leaves.iter() {
        snapshot_content_empty(fs, i, msg, &mut bad_btrees);
    }
    for i in nodes.delete_interior.iter() {
        snapshot_content_empty(fs, i.id, msg, &mut bad_btrees);
    }
    bad_btrees
}

fn node_set_no_keys(t: &TransAttempt<'_, '_>, id: u32, run: &Run<'_>) -> Result<(), BchError> {
    /* Before the get_mut below queues anything - see check_deletable() */
    check_no_data(t, id, "set_no_keys", Some(run))?;

    let mut s = get_mut_node(t, id)?;
    s.v_mut().set_state(c::bch_snapshot_state::SNAPSHOT_STATE_no_keys);
    Ok(())
}

fn normalize_child_pointers(s: &mut c::Snapshot) {
    if s.children[0].get() < s.children[1].get() {
        s.children.swap(0, 1);
    }
}

/// Every reason we decline to delete a snapshot node, decided without
/// touching anything.
///
/// These have to be answered before the deletion starts, never partway
/// through it. bkey_get_mut() queues its update the moment it's called, so a
/// refusal after the first one leaves the transaction holding a fragment of a
/// deletion, which the caller then drops on the floor.
///
/// And callers commit work ahead of the deletion that assumes it will happen:
/// delete_dead_interior_snapshots() rewrites every child's depth and skip[]
/// for the tree shape the deletion is going to produce, and commits that,
/// before it deletes a single node. A node refused after that leaves the tree
/// describing a deletion that never happened: depths one too small,
/// skiplists stepping over a node that's still there. Ancestry queries are
/// still correct (a skip still lands on a real ancestor), and
/// check_snapshots() recomputes depth; until it does,
/// snapshot_parent_child_consistent() can't corroborate those edges, and
/// won't repair real damage on them.
///
/// So this is separable from the deletion on purpose: a caller that is about
/// to commit to a set of nodes asks first, and drops the refused ones.
fn check_deletable(t: &TransAttempt<'_, '_>, id: u32, delete_interior: bool, run: Option<&Run<'_>>)
    -> Result<(), BchError>
{
    let fs = t.fs();

    check_no_data(t, id, if delete_interior { "interior delete" } else { "leaf delete" }, run)?;

    let s = lookup_node(t, id)?;
    let v = s.v();

    if state(v) == c::bch_snapshot_state::SNAPSHOT_STATE_deleted {
        trans_inconsistent!(t, "deleting snapshot node {id}: already in state deleted");
        return fs.throw(bch_errcode::BCH_ERR_EINVAL_snapshot_delete_already_deleted);
    }

    if v.children()[1] != 0 {
        let mut msg = LogMsg::new(fs);
        let _ = writeln!(msg, "deleting node with two children:");
        // Best effort: the refusal is the error, whatever the walk hits
        let _ = snapshot::tree_keys_to_text(&mut msg, t, id);
        nodes_to_text(&mut msg, run.map(|r| r.nodes), true);
        return fs.throw(bch_errcode::BCH_ERR_EINVAL_snapshot_delete_has_two_children);
    }

    if v.subvol() != 0 {
        // deletion path: see the deleted tombstone directly, as in
        // check_should_delete_leaf() - subvolume::get() would report it as
        // ENOENT_subvolume_deleted
        let subvol: c::Subvolume = t.bkey_get_val(c::btree_id::subvolumes, pos(0, v.subvol() as u64),
                                                  BtreeIterFlags::CACHED)?;

        if v.children()[0] != 0 ||
           (subvol.state().get() != c::bch_subvolume_state::SUBVOLUME_STATE_deleted.0 &&
            fs.version_upgrade_complete().0 >=
            c::bcachefs_metadata_version::bcachefs_metadata_version_per_dev_fragmentation_lru.0) {
            let mut msg = LogMsg::new(fs);
            let _ = writeln!(msg, "deleting node with bad subvolume pointer:");
            let _ = write!(msg, "{}", BkeySC::from(s.k_i()).to_text(fs));
            return fs.throw(bch_errcode::BCH_ERR_EINVAL_snapshot_delete_bad_subvol);
        }
    }

    if v.children()[0] != 0 && !delete_interior {
        let mut msg = LogMsg::new(fs);
        let _ = writeln!(msg, "deleting interior node {id} with child {} at runtime:", v.children()[0]);
        let _ = snapshot::tree_keys_to_text(&mut msg, t, id);
        nodes_to_text(&mut msg, run.map(|r| r.nodes), true);
        return fs.throw(bch_errcode::BCH_ERR_EINVAL_snapshot_delete_interior_at_runtime);
    }

    Ok(())
}

/// Splice node @id out of its tree, and tombstone it. Callers are
/// responsible for check_deletable() first: from here on we only fail on
/// errors that take the whole thing down, never on a refusal.
fn node_delete(t: &TransAttempt<'_, '_>, id: u32) -> Result<(), BchError> {
    let fs = t.fs();

    let mut s = get_mut_node(t, id)?;

    if s.v().subvol.get() != 0 {
        t.delete(c::btree_id::subvolumes, pos(0, s.v().subvol.get() as u64),
                 UpdateTriggerFlags::empty())?;
    }

    let parent_id = s.v().parent.get();
    let child_id  = s.v().children[0].get();

    if parent_id != 0 {
        let mut parent = get_mut_node(t, parent_id)?;

        /* find entry in parent->children for node being deleted */
        let Some(i) = (0..2).find(|&i| parent.v().children[i].get() == id) else {
            fs_inconsistent!(fs, "snapshot {parent_id} missing child pointer to {id}");
            return fs.throw(bch_errcode::BCH_ERR_ENOENT_snapshot);
        };

        parent.v_mut().children[i] = child_id.into();
        normalize_child_pointers(parent.v_mut());
    }

    if child_id != 0 {
        let mut child = get_mut_node(t, child_id)?;
        child.v_mut().parent = parent_id.into();
    }

    if parent_id == 0 {
        // We're deleting the root of a snapshot tree: update the
        // snapshot_tree entry to point to the new root, or delete it if this
        // is the last snapshot ID in this tree:
        let mut s_t: TransBkey<'_, '_, c::SnapshotTree> =
            t.bkey_get_mut(c::btree_id::snapshot_trees, pos(0, s.v().tree.get() as u64),
                           BtreeIterFlags::empty(), UpdateTriggerFlags::empty())?;

        if s.v().children[0].get() != 0 {
            s_t.v_mut().root_snapshot = s.v().children[0];
        } else {
            s_t.set_deleted();
        }
    }

    if fs.request_incompat_feature(
            c::bcachefs_metadata_version::bcachefs_metadata_version_snapshot_deletion_v2).is_ok() {
        // Retain parent/child pointers; don't destroy information if we have
        // to repair:
        let v = s.v_mut();
        v.subvol = 0u32.into();
        v.depth  = 0u32.into();
        v.skip   = [0u32.into(); 3];
        v.set_state(c::bch_snapshot_state::SNAPSHOT_STATE_deleted);
    } else {
        s.set_deleted();
    }

    // Nothing here touches this snapshot's accounting. Accounting is
    // derived: the triggers on the keys we just deleted are what took the
    // counters to zero. A counter still nonzero here is a bug to find, not a
    // number to correct - and check_no_data() above has already refused the
    // deletion if that's the case.
    Ok(())
}

/// Can we relink to this node? Only a deleted node has been spliced out of
/// the tree. no_keys is an emptied interior - kept in the tree until the next
/// remount precisely so ancestry still resolves - and carries child pointers
/// like any other node, so it is a perfectly ordinary neighbour to attach to.
fn in_tree(v: &c::Snapshot) -> bool {
    state_compat(v) != c::bch_snapshot_state::SNAPSHOT_STATE_deleted
}

/// Where to splice back in from below.
///
/// children[0] records where our splice took us out - but that node may have
/// been spliced out since, and each tombstone down there retains the child it
/// had when it died. So the node to reattach to is the nearest descendant
/// still in the tree, and the tombstones in between are our own splice being
/// undone one step at a time.
///
/// 0 when the chain runs out - nothing below us is in the tree, and the
/// caller relinks through the parent instead, as for a node that never had a
/// child.
///
/// Child ids are always below their parent's (bch2_snapshot_validate,
/// snapshot_child_bad), so the walk strictly descends and can't loop; a chain
/// that doesn't descend is corruption, and stops it.
fn relink_target(t: &TransAttempt<'_, '_>, mut child_id: u32) -> Result<u32, BchError> {
    while child_id != 0 {
        let child = match snapshot::lookup_key(t, child_id) {
            Err(e) if e.matches(c::ENOENT) => break,
            r => r?,
        };

        if in_tree(child.v()) {
            return Ok(child_id);
        }

        let next = child.v().children()[0];
        if next >= child_id {
            break;
        }
        child_id = next;
    }
    Ok(0)
}

/// Reinsert an undeleted node into the live tree: the inverse of the splice
/// in node_delete(). The tombstone retained its pointers as history: if the
/// parent's child slot (or the snapshot_tree root, for a deleted root)
/// currently holds the node relink_target() found below us, that's where the
/// splice took us out - take the slot back and take the child back. If the
/// slot holds us already, the deletion never got that far and there's
/// nothing to relink. If the topology has moved on, or an old wiped tombstone
/// retained nothing, the node revives unlinked and the tree pointer checks
/// handle it downstream.
pub fn node_undelete<'t>(t: &TransAttempt<'_, 't>, u: &mut TransBkey<'_, 't, c::Snapshot>)
    -> Result<(), BchError>
{
    let fs = t.fs();
    let id = u.k().p.offset as u32;

    let mut msg = LogMsg::new(fs);
    let _ = writeln!(msg, "{}", BkeySC::from(u.k_i()).to_text(fs));

    if u.v().children[1].get() != 0 {
        bch_err!(fs, "cannot undelete a node with two children");
        return fs.throw(bch_errcode::BCH_ERR_fsck_repair_unimplemented);
    }

    let child_id = relink_target(t, u.v().children[0].get())?;

    if child_id == 0 {
        // Nothing below us to splice in above: either we never had a child,
        // or the whole chain under us is out of the tree too. Either way our
        // retained parent pointer is what's left, so relink through it.
        // Handle the simple case, parent still live: revive in place, and if
        // our slot in the parent was cleared, the edge repairs complete it
        // from our side. A dead parent would need undeleting too -
        // unexpected, so fail rather than guess.
        //
        // A childless root has no parent to relink through and no children
        // to take back: it is the entire tree, so setting it live is the
        // whole repair. That is the shape of a filesystem that has never
        // been snapshotted, where refusing means refusing to mount at all.
        // Whether a snapshot_tree still points at it is the tree pointer
        // checks' business, as for any node that revives unlinked.
        let parent_id = u.v().parent.get();
        if parent_id != 0 {
            let mut parent = match t.bkey_get_mut::<c::Snapshot>(
                    c::btree_id::snapshots, pos(0, parent_id as u64),
                    BtreeIterFlags::empty(), UpdateTriggerFlags::empty()) {
                Err(e) if e.matches(c::ENOENT) => {
                    let _ = write!(msg, "cannot undelete: parent no longer exists");
                    return fs.throw(bch_errcode::BCH_ERR_fsck_repair_unimplemented);
                }
                r => r?,
            };

            if !in_tree(parent.v()) {
                let _ = write!(msg, "cannot undelete: parent is itself deleted");
                return fs.throw(bch_errcode::BCH_ERR_fsck_repair_unimplemented);
            }

            let pc = parent.v().children();
            if pc[0] != id && pc[1] != id {
                if pc[0] != 0 && pc[1] != 0 {
                    let _ = write!(msg, "cannot undelete: parent's child slots are full");
                    return fs.throw(bch_errcode::BCH_ERR_fsck_repair_unimplemented);
                }

                let i = if pc[0] == 0 { 0 } else { 1 };
                parent.v_mut().children[i] = id.into();
                normalize_child_pointers(parent.v_mut());
            }
        }

        u.v_mut().set_state(c::bch_snapshot_state::SNAPSHOT_STATE_live);
        return Ok(());
    }

    let mut child = match t.bkey_get_mut::<c::Snapshot>(
            c::btree_id::snapshots, pos(0, child_id as u64),
            BtreeIterFlags::empty(), UpdateTriggerFlags::empty()) {
        Err(e) if e.matches(c::ENOENT) => {
            bch_err!(fs, "cannot undelete: child no longer exists");
            return fs.throw(bch_errcode::BCH_ERR_fsck_repair_unimplemented);
        }
        r => r?,
    };

    let _ = writeln!(msg, "attaching to child  {}", BkeySC::from(child.k_i()).to_text(fs));

    let parent_id = child.v().parent.get();
    if parent_id != 0 && parent_id <= id {
        let _ = write!(msg, "cannot undelete: parent of child < node to undelete");
        return fs.throw(bch_errcode::BCH_ERR_fsck_repair_unimplemented);
    }

    child.v_mut().parent = id.into();

    if parent_id != 0 {
        let mut parent = match t.bkey_get_mut::<c::Snapshot>(
                c::btree_id::snapshots, pos(0, parent_id as u64),
                BtreeIterFlags::empty(), UpdateTriggerFlags::empty()) {
            Err(e) if e.matches(c::ENOENT) => {
                let _ = write!(msg, "cannot undelete: parent no longer exists");
                return fs.throw(bch_errcode::BCH_ERR_fsck_repair_unimplemented);
            }
            r => r?,
        };

        let _ = writeln!(msg, "attaching to parent {}", BkeySC::from(parent.k_i()).to_text(fs));

        if !in_tree(parent.v()) {
            let _ = write!(msg, "cannot undelete: parent is itself deleted");
            return fs.throw(bch_errcode::BCH_ERR_fsck_repair_unimplemented);
        }

        if let Some(i) = (0..2).find(|&i| parent.v().children[i].get() == child_id) {
            parent.v_mut().children[i] = id.into();
            normalize_child_pointers(parent.v_mut());
        }
    } else if child.v().tree.get() != 0 {
        match t.bkey_get_mut::<c::SnapshotTree>(
                c::btree_id::snapshot_trees, pos(0, child.v().tree.get() as u64),
                BtreeIterFlags::empty(), UpdateTriggerFlags::empty()) {
            Err(e) if e.matches(c::ENOENT) => {}
            Err(e) => return Err(e),
            Ok(mut s_t) => if s_t.v().root_snapshot.get() == child_id {
                s_t.v_mut().root_snapshot = id.into();
                let _ = writeln!(msg, "updated tree {}", BkeySC::from(s_t.k_i()).to_text(fs));
            },
        }
    }

    let tree = child.v().tree;
    let v = u.v_mut();
    v.parent = parent_id.into();
    v.tree   = tree;
    v.set_state(c::bch_snapshot_state::SNAPSHOT_STATE_live);

    v.depth = fs.snapshots.depth(parent_id).into();
    let mut skip = [0u32; 3];
    for s in &mut skip {
        *s = fs.snapshots.skiplist_get(parent_id);
    }
    skip.sort_unstable();
    v.skip = skip.map(Into::into);

    Ok(())
}

// If we have an unlinked inode in an internal snapshot node, and the inode
// really has been deleted in all child snapshots, how does this get cleaned
// up?
//
// first there is the problem of how keys that have been overwritten in all
// child snapshots get deleted (unimplemented?), but inodes may perhaps be
// special?
//
// also: unlinked inode in internal snapshot appears to not be getting deleted
// correctly if inode doesn't exist in leaf snapshots
//
// solution:
//
// for a key in an interior snapshot node that needs work to be done that
// requires it to be mutated: iterate over all descendent leaf nodes and copy
// that key to snapshot leaf nodes, where we can mutate it

// ── The key sweep ────────────────────────────────────────────────────────

/// If snapshot @id is one this run is emptying: its entry - live_child 0 for
/// a leaf.
fn snapshot_id_dying<'r>(run: &Run<'r>, id: u32) -> Option<&'r InteriorDelete> {
    let ret = Eytzinger1::find_by(run.dying, |d| d.id.cmp(&id)).map(|i| &run.dying[i]);

    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    match ret {
        None => {
            assert!(!run.nodes.delete_leaves.contains(&id));
            assert!(interior_delete_has_id(&run.nodes.delete_interior, id) == 0);
        }
        Some(d) if d.live_child == 0 => assert!(run.nodes.delete_leaves.contains(&id)),
        Some(d) => assert!(d.live_child == interior_delete_has_id(&run.nodes.delete_interior, id)),
    }

    ret
}

/// Remove a key from a dying/deleted snapshot node, migrating it to that
/// node's live descendant first when there is one (@live_child != 0): the key
/// is still visible to the descendant via inheritance, so dropping it
/// outright would lose data. Only copy it down if the descendant doesn't
/// already have its own key at that position. With no live descendant (a
/// leaf) the key is just deleted.
///
/// Shared by the deletion pass (process_key()) and the fsck repair
/// (check_key_has_snapshot()), which has C's iterator.
pub fn delete_dead_key<'a, 't>(
    t:          &TransAttempt<'a, 't>,
    iter:       &mut c::btree_iter,
    k:          BkeySC<'_>,
    live_child: u32,
) -> Result<(), BchError> {
    let fs = t.fs();

    if live_child != 0 {
        assert!(fs.snapshots.exists(live_child), "migrating to missing snapshot {live_child}");

        let btree = iter.btree_id();
        let dst = c::bpos { snapshot: live_child, ..k.k.p };

        let mut dst_iter = BtreeIter::new(t, btree, dst,
                                          BtreeIterFlags::ALL_SNAPSHOTS | BtreeIterFlags::INTENT);
        let damage = c::bch_bkey_type::KEY_TYPE_damage;

        match dst_iter.peek_slot(t)? {
            Some(dst_k) if btree == c::btree_id::damage &&
                           k.key_type() == damage && dst_k.key_type() == damage => {
                // Damage entries merge instead of dropping: the dying key can
                // hold counts recorded after the descendant's key was created,
                // so "descendant overwrote it" never applies:
                let new = damage::keys_merge(t, dst, dst_k, k)?;
                t.update(&dst_iter, &new, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
            }
            Some(dst_k) if !dst_k.is_deleted() => {}
            _ => {
                let mut new = t.bkey_make_mut_noupdate(k)?;
                new.k_mut().p = dst;
                t.update(&dst_iter, &new, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
            }
        }
    }

    t.delete_at_raw(iter, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)
}

fn process_key(t: &TransAttempt<'_, '_>, iter: &BtreeIter<'_>, k: BkeySC<'_>, run: &Run<'_>)
    -> Result<(), BchError>
{
    let fs = t.fs();

    // SAFETY: the loop's iterator, which it hands us shared: C's, for C's
    // repair to delete at, as C's sweep did - nothing else uses it meanwhile
    let raw = unsafe { &mut *iter.raw() };

    if !fs.snapshots.exists(k.k.p.snapshot) &&
       check_snapshots::check_key_has_snapshot(t, Some(&mut *raw), k)? {
        return t.commit_lazy(CommitFlags::NO_ENOSPC);
    }

    let Some(dying) = snapshot_id_dying(run, k.k.p.snapshot) else {
        return Ok(());
    };

    delete_dead_key(t, raw, k, dying.live_child)
}

fn delete_dead_snapshot_keys_v1_btree(trans: &BtreeTrans<'_>, btree: c::btree_id, run: &Run<'_>)
    -> Result<(), BchError>
{
    let fs = trans.fs();
    let d = &fs.snapshots.delete;
    let res = DiskReservation::new(fs);

    let mut iter = BtreeIter::new(trans, btree, POS_MIN,
                                  BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);
    iter.for_each_commit(trans, Some(&res), CommitFlags::NO_ENOSPC, |t, iter, k| {
        let _ = d.progress.update(t, iter);

        res.put();
        process_key(t, iter, k, run)
    })
}

fn delete_dead_snapshot_keys_v1(trans: &BtreeTrans<'_>, run: &Run<'_>) -> Result<(), BchError> {
    let fs = trans.fs();

    fs.snapshots.delete.start_sweep(fs, c"delete_dead_snapshot_keys_v1",
                                    crate::BTREE_HAS_SNAPSHOTS_MASK, 1);

    for &btree in crate::BTREE_IDS_KNOWN {
        if crate::BTREE_HAS_SNAPSHOTS_MASK & (1u64 << btree.0) != 0 && btree != c::btree_id::inodes {
            delete_dead_snapshot_keys_v1_btree(trans, btree, run)?;
        }
    }

    /*
     * fsck assumes that we'll process the inodes btree last:
     */
    delete_dead_snapshot_keys_v1_btree(trans, c::btree_id::inodes, run)
}

fn delete_dead_snapshot_keys_range(
    trans: &BtreeTrans<'_>,
    res:   &DiskReservation<'_>,
    btree: c::btree_id,
    start: c::bpos,
    end:   c::bpos,
    run:   &Run<'_>,
) -> Result<(), BchError> {
    let flags = BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS;
    let mut iter = BtreeIter::new(trans, btree, start, flags);
    iter.for_each_max_commit(trans, end, flags, Some(res), CommitFlags::NO_ENOSPC, |t, iter, k| {
        res.put();
        process_key(t, iter, k, run)
    })
}

fn delete_dead_snapshot_keys_v2(trans: &BtreeTrans<'_>, run: &Run<'_>) -> Result<(), BchError> {
    let fs = trans.fs();
    let d = &fs.snapshots.delete;
    let res = DiskReservation::new(fs);

    d.start_sweep(fs, c"delete_dead_snapshot_keys_v2", 1u64 << c::btree_id::inodes.0, 2);

    let mut iter = BtreeIter::new(trans, c::btree_id::inodes, POS_MIN,
                                  BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);

    // First, delete extents/dirents/xattrs and damage keys
    //
    // If an extent/dirent/xattr is present in a given snapshot ID an inode
    // must also be present in that same snapshot ID (and a damage key has an
    // inode at its exact position - enforced by check_damage), so we can use
    // this to greatly accelerate scanning:
    loop {
        let p = lockrestart_do(trans, |t| {
            let p = iter.peek(t)?.map(|k| k.k.p);
            if p.is_some() {
                let _ = d.progress.update(t, &iter);
            }
            Ok(p)
        })?;
        let Some(p) = p else { break };

        if snapshot_id_dying(run, p.snapshot).is_some() {
            let start = pos(p.offset, 0);
            let end   = pos(p.offset, u64::MAX);

            delete_dead_snapshot_keys_range(trans, &res, c::btree_id::extents, start, end, run)?;
            delete_dead_snapshot_keys_range(trans, &res, c::btree_id::dirents, start, end, run)?;
            delete_dead_snapshot_keys_range(trans, &res, c::btree_id::xattrs, start, end, run)?;
            delete_dead_snapshot_keys_range(trans, &res, c::btree_id::damage,
                                            pos(0, p.offset), spos(0, p.offset, u32::MAX), run)?;

            iter.set_pos(pos(0, p.offset + 1));
        } else {
            iter.advance();
        }
    }

    // The scan above located keys through the inodes btree, so deleting
    // inode keys destroys the index it used: anything left in the other
    // btrees at a dying snapshot becomes unfindable, by this pass and by every
    // later one. Check while it's still repairable - the check_no_data
    // licenses at the end of deletion only fire once the inodes are already
    // gone.
    let mut msg = Printbuf::new();
    let mut bad_btrees = dying_snapshots_content_btrees(fs, run.nodes, &mut msg);

    if bad_btrees != 0 {
        // Accounting is derived from the keys themselves, so it doesn't go
        // through the index above and can see what that scan missed. The v1
        // scan doesn't use the index either - rescan with it, on the btrees
        // accounting named and no others.
        let _ = write!(msg, "\nfalling back to the v1 scan");
        log::print_str(fs, LogLevel::NOTICE, &mut msg);

        // Retarget the progress indicator: it was initialized for the inodes
        // btree, which is not what we're about to walk. Nothing after this
        // loop updates progress, so this is the last word, and version says
        // the run degraded.
        d.start_sweep(fs, c"delete_dead_snapshot_keys_v2", bad_btrees, 1);

        for &btree in crate::BTREE_IDS_KNOWN {
            if bad_btrees & (1u64 << btree.0) != 0 {
                delete_dead_snapshot_keys_v1_btree(trans, btree, run)?;
            }
        }

        msg = Printbuf::new();
        bad_btrees = dying_snapshots_content_btrees(fs, run.nodes, &mut msg);
    }

    if bad_btrees != 0 {
        // v1 found nothing and the keys are still accounted, so nothing is
        // hiding behind a missing inode - the count is what's wrong. No
        // content passes: their key_has_snapshot repairs exist to uphold the
        // invariant v1 doesn't need. Refuse anyway - the inode keys below are
        // the only way left to find anything that is in fact stranded.
        let ret = passes::run_explicit(fs, &mut msg, BCH_RECOVERY_PASS_check_allocations,
                                       c::bch_run_recovery_pass_flags::from_bits_retain(0));

        bch_err!(fs, "{}", msg.as_str());

        rewind_is_fine(ret)?;
        return fs.throw(bch_errcode::BCH_ERR_EINVAL_snapshot_delete_with_data);
    }

    if snapshot_delete_bail_before_inodes.get() {
        bch_notice!(fs, "delete_dead_snapshot_keys_v2: cancelling before inode deletion (module param)");
        return fs.throw(bch_errcode::BCH_ERR_snapshot_delete_cancelled);
    }

    /* Then the inodes */

    let mut iter = BtreeIter::new(trans, c::btree_id::inodes, POS_MIN,
                                  BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);
    iter.for_each_commit(trans, Some(&res), CommitFlags::NO_ENOSPC, |t, iter, k| {
        res.put();
        process_key(t, iter, k, run)
    })
}

// ── Choosing what to delete ──────────────────────────────────────────────

/// What collection gathers - the run's, until it publishes the nodes.
#[derive(Default)]
struct Collection {
    nodes:   DeleteNodes,
    no_keys: KVVec<InteriorDelete>,
}

/// Whether leaf @k, @v its value, is to be deleted.
fn check_should_delete_leaf(t: &TransAttempt<'_, '_>, k: BkeySC<'_>, v: &c::Snapshot)
    -> Result<bool, BchError>
{
    let fs = t.fs();

    let mut buf = Printbuf::new();
    let _ = write!(buf, "{}", k.to_text(fs));

    let s = state(v);
    if s == c::bch_snapshot_state::SNAPSHOT_STATE_live {
        Ok(false)
    } else if s == c::bch_snapshot_state::SNAPSHOT_STATE_will_delete {
        if v.subvol.get() == 0 {
            // A will_delete leaf with no subvolume backref is safe to delete
            // once check_subvols has confirmed no live subvolume points at a
            // non-live snapshot (it halts recovery if one does). Require
            // that pass - it can run online - rather than scanning the
            // subvolumes btree here, or trusting the filesystem version,
            // which can't distinguish a legacy will_delete leaf from real
            // corruption.
            passes::require(fs, &mut buf, BCH_RECOVERY_PASS_check_subvols)?;
        } else {
            // Raw lookup, not subvolume::get(): this is the deletion path,
            // the one caller that must see the deleted tombstone rather than
            // have it reported as ENOENT.
            let subvol: c::Subvolume = t.bkey_get_val(c::btree_id::subvolumes,
                                                      pos(0, v.subvol.get() as u64),
                                                      BtreeIterFlags::CACHED)?;

            if subvol.state().get() != c::bch_subvolume_state::SUBVOLUME_STATE_deleted.0 {
                fs_inconsistent!(fs, "snapshot marked for deletion but subvolume not marked for deletion\n{}",
                                 buf.as_str());
                return Ok(false);
            }

            if subvol.snapshot() as u64 != k.k.p.offset {
                fs_inconsistent!(fs, "snapshot marked for deletion but subvolume does not point back\n{}",
                                 buf.as_str());
                return Ok(false);
            }
        }
        Ok(true)
    } else if s == c::bch_snapshot_state::SNAPSHOT_STATE_no_keys {
        // An emptied interior node whose children have all been deleted is
        // normally reaped in the same pass that deletes its last child - each
        // node deletion is its own commit, so a childless no_keys node is
        // what a crash in between leaves. Shouldn't occur otherwise; handle
        // it gracefully - it's empty by construction and node_delete
        // re-verifies no-data:
        fsck_err!(t, id::snapshot_no_keys_childless,
                  "childless no_keys snapshot node, deleting:\n{}", buf.as_str())
    } else {
        fs_inconsistent!(fs, "snapshot leaf in invalid state\n{}", buf.as_str());
        Ok(false)
    }
}

/// @list, with @id if it wasn't already.
fn add_nodup(fs: &Fs, list: &mut KVVec<u32>, id: u32) -> Result<(), BchError> {
    if !list.contains(&id) {
        list.push(id, GFP_KERNEL).inspect_err(|_| {
            bch_err!(fs, "error reallocating snapshot_id_list (size {})", list.capacity());
        })?;
    }
    Ok(())
}

/// Sort one node: dead (no live descendant - keys are dropped) or redundant
/// (exactly one live child - keys migrate to live_child). Two live children:
/// nothing to do.
///
/// The caller scans in ascending id order and a node's id is always below
/// its parent's, so children are sorted before parents. Both cases need that:
///
/// - A node whose children are all already on delete_leaves is itself dead
///   and joins them, so a dead subtree accumulates bottom-up. It's torn down
///   in that same order later, each node childless by its turn because
///   deleting a child splices this node's pointer to zero.
///
/// - A child that's itself redundant already has its own live_child
///   recorded, so the lookup below returns the terminal of the whole collapse
///   chain rather than the next node down, and keys move there in one hop.
fn check_should_delete_snapshot(t: &TransAttempt<'_, '_>, k: BkeySC<'_>, coll: &mut Collection)
    -> Result<(), BchError>
{
    let Some(v) = snapshot::val(k) else { return Ok(()) };
    let fs = t.fs();
    let id = k.k.p.offset as u32;

    if state(&v) == c::bch_snapshot_state::SNAPSHOT_STATE_deleted {
        return Ok(());
    }

    if v.children[0].get() == 0 && !check_should_delete_leaf(t, k, &v)? {
        return Ok(());
    }

    // The loop's per-key commit can restart after the pushes below and
    // replay this body - the lists aren't transactional, so every push must
    // be idempotent: nodup adds and has_id guards, or collection double-adds
    // on replay. (Repairs invalidate collected state differently: the
    // deletion path resets the lists wholesale and rescans.)
    let (mut live_child, mut nr_live_children) = (0, 0);

    for child in v.children.map(|c| c.get()) {
        if child != 0 && !coll.nodes.delete_leaves.contains(&child) {
            nr_live_children += 1;

            live_child = match interior_delete_has_id(&coll.nodes.delete_interior, child) {
                0 => match interior_delete_has_id(&coll.no_keys, child) {
                    0  => child,
                    lc => lc,
                },
                lc => lc,
            };
        }
    }

    if nr_live_children == 2 {
        return Ok(());
    }

    // The resolved live child is about to license key migration and a
    // splice: if it isn't in the table, is self-referential, or isn't a
    // descendant, the topology is damaged - schedule repair and bail out of
    // deletion, which runs again once check_snapshots has fixed it:
    if live_child != 0 &&
       (live_child == id ||
        !fs.snapshots.exists(live_child) ||
        !snapshot::is_ancestor(t, live_child, id)) {
        let mut msg = LogMsg::new(fs);

        let _ = writeln!(msg, "snapshot deletion found damaged topology (resolved live child {live_child}):");
        let _ = write!(msg, "{}", k.to_text(fs));

        passes::run_explicit(fs, &mut msg, BCH_RECOVERY_PASS_check_snapshots,
                             c::bch_run_recovery_pass_flags::from_bits_retain(0))?;
        return fs.throw(bch_errcode::BCH_ERR_EINVAL_snapshot_delete_bad_topology);
    }

    let no_keys = state(&v) == c::bch_snapshot_state::SNAPSHOT_STATE_no_keys;

    if !no_keys {
        add_nodup(fs, &mut coll.nodes.deleting_from_trees, fs.snapshots.tree(id))?;
    }

    if nr_live_children == 0 {
        add_nodup(fs, &mut coll.nodes.delete_leaves, id)?;
    } else {
        let n = InteriorDelete { id, live_child };

        // We're not doing any processing for NO_KEYS snapshot nodes, but we
        // still track them so that we can find the correct live_child when
        // deleting parents, above:
        let list = if !no_keys { &mut coll.nodes.delete_interior } else { &mut coll.no_keys };
        if interior_delete_has_id(list, n.id) == 0 {
            list.push(n, GFP_KERNEL)?;
        }
    }

    Ok(())
}

/// @id's @n'th ancestor, counting none of the nodes in @skip.
fn nth_parent_skip(fs: &Fs, mut id: u32, n: u32, skip: &[InteriorDelete]) -> u32 {
    let parent = |id| fs.snapshots.parent(id).unwrap_or(0);

    while interior_delete_has_id(skip, id) != 0 {
        id = parent(id);
    }

    for _ in 0..n {
        loop {
            id = parent(id);
            if interior_delete_has_id(skip, id) == 0 {
                break;
            }
        }
    }
    id
}

/// Node @k's depth and skiplist, for the tree shape deleting @deleted
/// produces.
fn fix_child_of_deleted_snapshot(
    t:       &TransAttempt<'_, '_>,
    iter:    &BtreeIter<'_>,
    k:       BkeySC<'_>,
    deleted: &[InteriorDelete],
) -> Result<(), BchError> {
    let fs = t.fs();
    let id = k.k.p.offset as u32;

    let BkeySCTyped::snapshot(k) = k.typed() else { return Ok(()) };

    if interior_delete_has_id(deleted, id) != 0 {
        return Ok(());
    }

    let mut s = t.bkey_make_mut_noupdate(k)?;

    let nr_deleted_ancestors = deleted.iter()
        .filter(|i| fs.snapshots.same_tree(id, i.id) && snapshot::is_ancestor(t, id, i.id))
        .count() as u32;

    if nr_deleted_ancestors == 0 {
        return Ok(());
    }

    let v = s.v_mut();
    let depth = v.depth.get().wrapping_sub(nr_deleted_ancestors);
    v.depth = depth.into();

    if depth == 0 {
        v.skip = [0u32.into(); 3];
    } else {
        let parent = fs.snapshots.parent(id).unwrap_or(0);

        let mut skip = v.skip.map(|s| s.get());
        for s in &mut skip {
            if interior_delete_has_id(deleted, *s) != 0 {
                let n = if depth > 1 {
                    crate::util::kernel::random_u64_below((depth - 1) as u64) as u32
                } else {
                    0
                };
                *s = nth_parent_skip(fs, parent, n, deleted);
            }
        }
        skip.sort_unstable();
        v.skip = skip.map(Into::into);
    }

    t.update(iter, &s, UpdateTriggerFlags::empty())
}

// ── Deletion ─────────────────────────────────────────────────────────────

fn delete_dead_snapshots_locked(fs: &Fs) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    let d = &fs.snapshots.delete;

    // For every snapshot node: If we have no live children and it's not
    // pointed to by a subvolume, delete it.
    //
    // The per-key commit is for the childless-no_keys fsck_err: its fix is
    // out-of-band (the node is only collected here, deleted later), but the
    // fsck_err queues its repair log entry on this trans - which would
    // otherwise never commit, only be dropped at the next trans_begin (the
    // iter.c dropped-updates WARN):
    let mut coll = Collection::default();
    let mut iter = BtreeIter::new(&trans, c::btree_id::snapshots, POS_MIN, BtreeIterFlags::empty());
    iter.for_each_commit(&trans, None, CommitFlags::empty(), |t, _, k| {
        check_should_delete_snapshot(t, k, &mut coll)
    })?;

    if coll.nodes.delete_leaves.is_empty() && coll.nodes.delete_interior.is_empty() {
        return Ok(());
    }

    // Eytzinger trees with 1-based indexing are faster than 0-based indexing
    // due to better cacheline alignment:
    let mut dying = KVVec::new();
    dying.push(InteriorDelete::default(), GFP_KERNEL)?;
    for &i in coll.nodes.delete_interior.iter() {
        dying.push(i, GFP_KERNEL)?;
    }
    for &id in coll.nodes.delete_leaves.iter() {
        dying.push(InteriorDelete { id, live_child: 0 }, GFP_KERNEL)?;
    }
    Eytzinger1::sort_by(&mut dying, |l, r| l.id.cmp(&r.id));

    // Published: sysfs reads it from here, as we do.
    *d.nodes.write() = Some(coll.nodes);
    let nodes = d.nodes.read();
    let run = Run {
        nodes:   nodes.as_ref().expect("just published"),
        no_keys: &coll.no_keys,
        dying:   &dying,
    };

    let mut buf = Printbuf::new();
    nodes_to_text(&mut buf, Some(run.nodes), false);
    commit_do(&trans, None, CommitFlags::empty(), |t| t.log_msg(&mut buf))?;

    let node_data = delete_data_to_text(fs, run.nodes);
    bch_info!(fs, "{}", node_data.as_str());

    if fs.request_incompat_feature(
            c::bcachefs_metadata_version::bcachefs_metadata_version_snapshot_deletion_v2).is_ok() {
        delete_dead_snapshot_keys_v2(&trans, &run)?;
    } else {
        delete_dead_snapshot_keys_v1(&trans, &run)?;
    }

    // Refusals first, and a refusal stops the whole deletion: nodes go one
    // commit at a time, so refusing partway leaves the ones already deleted
    // describing a tree shape the rest of the deletion no longer produces.
    // The refusal schedules its own repair - deletion runs again after it.
    //
    // This can't precede the migration above: what the check reads is
    // whether the keys that were supposed to move actually did.
    for &i in run.nodes.delete_leaves.iter() {
        lockrestart_do(&trans, |t| check_no_data(t, i, "leaf delete", Some(&run)))?;
    }

    for i in run.nodes.delete_interior.iter() {
        lockrestart_do(&trans, |t| check_no_data(t, i.id, "set_no_keys", Some(&run)))?;
    }

    for &i in run.nodes.delete_leaves.iter() {
        commit_do(&trans, None, CommitFlags::empty(), |t| {
            check_deletable(t, i, false, Some(&run))?;
            node_delete(t, i)
        })?;
    }

    for i in run.nodes.delete_interior.iter() {
        commit_do(&trans, None, CommitFlags::empty(), |t| node_set_no_keys(t, i.id, &run))?;
    }

    Ok(())
}

/// A refusal means "repair first, then retry", not failure: the check that
/// refused has scheduled the passes that fix what it found, and deletion is
/// rescheduled the next time a dying snapshot is seen (bch2_mark_snapshot,
/// check_snapshot_needs_deletion()). Nothing has been done, so there is
/// nothing wrong with the filesystem that going read-only would protect.
///
/// These run as recovery passes, and bch2_run_recovery_passes() returns the
/// first pass error it sees - which at startup fails the mount. So the
/// refusals stop here.
fn refused(ret: Result<(), BchError>) -> Result<(), BchError> {
    match ret {
        Err(e) if e == bch_errcode::BCH_ERR_EINVAL_snapshot_delete_with_data.into() ||
                  e == bch_errcode::BCH_ERR_EINVAL_snapshot_delete_bad_topology.into() => Ok(()),
        r => r,
    }
}

/// Delete dead snapshots, regardless of the auto_snapshot_deletion option:
/// for sysfs's force-trigger.
///
/// Serialization is recovery.run_lock: the delete_dead_snapshots pass runs
/// under it (the framework holds run_lock while running passes), and the
/// sysfs force-trigger takes it explicitly. So no separate lock is needed,
/// and passes never run this concurrently.
pub fn delete_dead_snapshots_forced(fs: &Fs) -> Result<(), BchError> {
    let d = &fs.snapshots.delete;

    passes::assert_run_lock_held(fs);

    d.progress.reset();

    let ret = refused(delete_dead_snapshots_locked(fs));

    *d.nodes.write() = None;

    passes::set_no_ratelimit(fs, BCH_RECOVERY_PASS_check_snapshots);

    ret
}

#[c_export]
pub fn __bch2_delete_dead_snapshots(c: &Opaque<c::bch_fs>) -> core::ffi::c_int {
    ret_to_c(delete_dead_snapshots_forced(Fs::from_c(c)))
}

fn delete_dead_snapshots(fs: &Fs) -> Result<(), BchError> {
    if fs.opts().auto_snapshot_deletion == 0 {
        return Ok(());
    }

    delete_dead_snapshots_forced(fs)
}

crate::recovery_pass!(bch2_delete_dead_snapshots => delete_dead_snapshots);

fn get_dead_interior_snapshots(k: BkeySC<'_>, delete: &mut KVVec<InteriorDelete>) -> Result<(), BchError> {
    let Some(v) = snapshot::val(k) else { return Ok(()) };

    if state(&v) == c::bch_snapshot_state::SNAPSHOT_STATE_no_keys {
        let (mut live_child, mut nr_live_children) = (0, 0);

        for child in v.children.map(|c| c.get()) {
            if child != 0 {
                nr_live_children += 1;
                live_child = match interior_delete_has_id(delete, child) {
                    0  => child,
                    lc => lc,
                };
            }
        }

        if nr_live_children != 1 {
            return Ok(());
        }

        delete.push(InteriorDelete { id: k.k.p.offset as u32, live_child }, GFP_KERNEL)?;
    }
    Ok(())
}

fn collect_dead_interior_snapshots(trans: &BtreeTrans<'_>) -> Result<KVVec<InteriorDelete>, BchError> {
    let mut delete = KVVec::new();
    let mut iter = BtreeIter::new(trans, c::btree_id::snapshots, POS_MIN, BtreeIterFlags::empty());
    iter.for_each(trans, |_, _, k| get_dead_interior_snapshots(k, &mut delete))?;
    Ok(delete)
}

fn delete_dead_interior_snapshots(fs: &Fs) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);

    let mut delete = collect_dead_interior_snapshots(&trans)?;

    if !delete.is_empty() {
        {
            let mut msg = LogMsg::with_level(fs, LogLevel::NOTICE);

            let _ = writeln!(msg, "Deleting interior snapshot nodes forces check_snapshots:");
            passes::run_explicit(fs, &mut msg, BCH_RECOVERY_PASS_check_snapshots,
                                 c::bch_run_recovery_pass_flags::from_bits_retain(0))?;
        }

        // This is also what makes the non-atomic depth rewrite below safe to
        // retry: check_snapshots re-derives every depth from the parent
        // chain. If we crashed partway through that rewrite, the surviving
        // children carry decremented depths while their parent pointers are
        // still untouched - so this restores them, and the decrement lands
        // exactly once per run.
        check_snapshots::check_snapshots_trans(&trans)?;

        // check_snapshots is licensed to change node states (e.g. repairing a
        // stale-state legacy tombstone to deleted), so the pre-check
        // collection is only good for deciding whether to run it - re-collect
        // for the authoritative delete list:
        delete = collect_dead_interior_snapshots(&trans)?;
    }

    if !delete.is_empty() {
        // Refusals first: the fixup below commits every surviving child's new
        // depth and skip[] for the tree shape these deletions produce, so a
        // node refused after it leaves the children describing a deletion
        // that never happened - skip[] pointing above parent, which
        // validation rejects on the next write.
        for i in delete.iter() {
            lockrestart_do(&trans, |t| check_deletable(t, i.id, true, None))?;
        }

        // Fixing children of deleted snapshots can't be done completely
        // atomically, if we crash between here and when we delete the
        // interior nodes some depth fields will be off - the check_snapshots
        // above is what puts them back on the next run.
        //
        // So for the whole span between this loop and the delete loop below,
        // a child carries its new depth and its old parent pointer: depth !=
        // parent's depth + 1 is a state this pass passes through, not an
        // invariant.
        let mut iter = BtreeIter::new(&trans, c::btree_id::snapshots, POS_MIN, BtreeIterFlags::INTENT);
        iter.for_each_commit(&trans, None, CommitFlags::NO_ENOSPC, |t, iter, k| {
            fix_child_of_deleted_snapshot(t, iter, k, &delete)
        })?;

        for i in delete.iter() {
            let ret = commit_do(&trans, None, CommitFlags::empty(), |t| node_delete(t, i.id));
            if !ret.is_err_and(|e| e.matches(c::EROFS)) {
                let _ = bch_err_msg!(fs, ret, "deleting snapshot {}", i.id);
            }
            ret?;
        }
    }

    Ok(())
}

fn delete_dead_interior_snapshots_pass(fs: &Fs) -> Result<(), BchError> {
    if fs.opts().auto_snapshot_deletion == 0 {
        return Ok(());
    }

    refused(delete_dead_interior_snapshots(fs))
}

crate::recovery_pass!(bch2_delete_dead_interior_snapshots => delete_dead_interior_snapshots_pass);

/// If there's one child, it's redundant and keys will be moved to the child
fn interior_snapshot_needs_delete(v: &c::Snapshot) -> bool {
    v.children.iter().filter(|c| c.get() != 0).count() == 1
}

/// For check_snapshots' walk (snapshot.c): count the emptied interior nodes,
/// and schedule deletion for a node that needs it.
fn check_snapshot_needs_deletion(t: &TransAttempt<'_, '_>, k: BkeySC<'_>, nr_empty_interior: &mut u32) {
    let Some(v) = snapshot::val(k) else { return };
    let s = state_compat(&v);

    if s == c::bch_snapshot_state::SNAPSHOT_STATE_deleted {
        return;
    }

    if s == c::bch_snapshot_state::SNAPSHOT_STATE_no_keys {
        *nr_empty_interior += 1;
    } else if s == c::bch_snapshot_state::SNAPSHOT_STATE_will_delete ||
              interior_snapshot_needs_delete(&v) {
        // Schedule the deleter through the recovery-pass machinery, like
        // bch2_mark_snapshot - this catches the interior single-child case,
        // which mark_snapshot (will_delete only) doesn't. Ephemeral +
        // best-effort: ignore the return.
        let mut buf = Printbuf::new();
        let _ = passes::run_explicit(t.fs(), &mut buf, BCH_RECOVERY_PASS_delete_dead_snapshots,
                                     c::bch_run_recovery_pass_flags::RUN_RECOVERY_PASS_ephemeral);
    }
}

#[c_export]
pub fn bch2_check_snapshot_needs_deletion(
    trans:             &Opaque<c::btree_trans>,
    k:                 BkeySC<'_>,
    nr_empty_interior: &mut u32,
) -> core::ffi::c_int {
    check_snapshot_needs_deletion(&unsafe { BtreeTrans::from_c(trans).attempt_in_progress() },
                                  k, nr_empty_interior);
    0
}
