// SPDX-License-Identifier: GPL-2.0

//! Snapshot fsck: the passes over the snapshots and snapshot_trees btrees.
//!
//! check_snapshot_trees: every tree key names a live root and master subvol.
//!
//! check_snapshots: per node - check_state() recovers the state field itself;
//! check_has_data() and check_deleted() hold a non-live state up against the
//! accounting, the child snapshots and the subvolume; then the topology checks
//! (edges, tree pointer, depth, skiplists, subvol backref).
//!
//! reconstruct_snapshots: rebuild missing nodes from the keys that reference
//! them.
//!
//! check_key_has_snapshot: per-key repair for keys whose snapshot is missing
//! or dead; also called at runtime, through bch2_check_key_has_snapshot().
//!
//! Before repairing a state, enumerate which writers can produce it.
//! Unconstructible combinations are rejected at commit
//! (bch2_snapshot_validate()); real damage is repaired toward what the rest of
//! the metadata agrees on - the child snapshots, the subvolume (deletion
//! tombstones it in the transaction that condemns its snapshot), the
//! per-snapshot accounting (nothing we write deletes a node with data).
//! Ambiguity fail-stops rather than guessing.
//!
//! The node being checked is a CowKey: one copy, read by every check and
//! written by every repair, so one repair can't lose another's.

use crate::accounting;
use crate::btree::bkey::{pos, BkeyI, BkeySC, POS_MAX, POS_MIN, SPOS_MAX};
use crate::btree::cow_key::CowKey;
use crate::btree::iter::{
    commit_do, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, TransAttempt, TransBkey,
    UpdateTriggerFlags,
};
use crate::c;
use crate::c::bch_recovery_pass::*;
use crate::c::snapshot_id_state as IdState;
use crate::check;
use crate::errcode::{bch_errcode, BchError, Found};
use crate::fs::Fs;
use crate::init::error::id;
use crate::init::passes;
use crate::init::progress::Progress;
use crate::inode;
use crate::snapshots::{snapshot, subvolume};
use crate::util::alloc::{flags::GFP_KERNEL, KVVec};
use crate::util::Printbuf;
use crate::{bch_err, bch_err_fn, fsck_err, fsck_err_on, fsck_err_report, inode_fsck_err};
use core::fmt;
use cstruct_macros::c_export;

/// Snapshot node @id's key, None if there's none: fsck can't assume a node
/// exists, and absence as a value can't be returned as an error by mistake.
/// The whole key: the topology checks print nodes they didn't start from.
fn lookup_node(t: &TransAttempt<'_, '_>, id: u32) -> Result<Option<BkeyI<c::Snapshot>>, BchError> {
    snapshot::lookup_key(t, id).found()
}

/// Snapshot node @id's key, as a mutable copy queued for the commit.
fn get_mut_node<'a, 't>(t: &TransAttempt<'a, 't>, id: u32)
    -> Result<TransBkey<'a, 't, c::Snapshot>, BchError>
{
    t.bkey_get_mut(c::btree_id::snapshots, pos(0, id as u64), BtreeIterFlags::empty(),
                   UpdateTriggerFlags::empty())
}

/// The id - offset - of the first key in @btree @pred accepts: for the
/// btrees keyed by id, subvolumes and snapshot trees.
fn find_key(
    t:        &TransAttempt<'_, '_>,
    btree:    c::btree_id,
    mut pred: impl FnMut(BkeySC<'_>) -> bool,
) -> Result<Option<u32>, BchError> {
    let mut iter = BtreeIter::new(t, btree, POS_MIN, BtreeIterFlags::empty());
    Ok(iter.find_max_norestart(t, SPOS_MAX, |_, k| Ok(pred(k)))?
       .map(|k| k.k.p.offset as u32))
}

/// A subvolume claiming snapshot @id - to restore a wiped backref.
fn subvol_claiming(t: &TransAttempt<'_, '_>, id: u32) -> Result<Option<u32>, BchError> {
    find_key(t, c::btree_id::subvolumes,
             |k| subvolume::val(k).is_some_and(|s| s.snapshot() == id))
}

/// A state field by name; "(invalid state)" if it isn't one.
struct StateName(Option<c::bch_snapshot_state>);

impl fmt::Display for StateName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(s) => write!(f, "{s}"),
            None    => f.write_str("(invalid state)"),
        }
    }
}

/* check_snapshot_trees: */

/// A subvolume for snapshot tree @root to name as its master: a
/// non-snapshot subvolume in the tree, or failing that the oldest one, made a
/// non-snapshot subvolume. None if the tree has none.
fn tree_master_subvol(t: &TransAttempt<'_, '_>, root: u32) -> Result<Option<u32>, BchError> {
    let fs = t.fs();

    let found = find_key(t, c::btree_id::subvolumes, |k| {
        subvolume::val(k).is_some_and(|s| snapshot::is_ancestor(t, s.snapshot(), root) && !s.snap())
    })?;
    if found.is_some() {
        return Ok(found);
    }

    let Some(oldest) = fs.snapshots.oldest_subvol(root) else { return Ok(None) };
    let Some(mut u) = t.bkey_get_mut::<c::Subvolume>(c::btree_id::subvolumes, pos(0, oldest as u64),
                                                     BtreeIterFlags::empty(),
                                                     UpdateTriggerFlags::empty()).found()? else {
        return Ok(None);
    };
    u.v_mut().set_snap(false);
    Ok(Some(oldest))
}

/// Tree key @k must name a root that points back, or it's deleted; and its
/// master must be a non-snapshot subvolume in the tree, or it's corrected.
fn check_snapshot_tree<'t>(
    t:    &TransAttempt<'_, 't>,
    iter: &BtreeIter<'t>,
    k:    BkeySC<'_>,
) -> Result<(), BchError> {
    let fs = t.fs();

    let Some(k) = k.arm::<c::SnapshotTree>() else { return Ok(()) };
    let st = k.v();
    let root_id = st.root_snapshot.get();

    let root = match snapshot::lookup_key(t, root_id) {
        Err(e) if !e.matches(c::ENOENT) => return Err(e),
        r => r,
    };

    let bad = match &root {
        Err(_) => true,
        Ok(r)  => root_id != fs.snapshots.root(root_id) || k.k.p.offset as u32 != r.v.tree(),
    };
    if bad {
        let mut found = Printbuf::new();
        match &root {
            Err(e) => write!(found, "({})", e.msg()),
            Ok(r)  => write!(found, "{}", BkeySC::from(r.k_i()).to_text(fs)),
        }
        if fsck_err!(t, id::snapshot_tree_to_missing_snapshot,
                     "snapshot tree points to missing/incorrect snapshot:\n{}\n{}",
                     k.to_text(fs), found)? {
            return t.delete_at(iter, UpdateTriggerFlags::empty());
        }
    }

    let master = st.master_subvol.get();
    if master == 0 {
        return Ok(());
    }

    let repair = match subvolume::get(t, master, false).found()? {
        None => fsck_err!(t, id::snapshot_tree_to_missing_subvol,
                          "snapshot tree points to missing subvolume:\n{}", k.to_text(fs))?,
        Some(s) =>
            fsck_err_on!(t, !snapshot::is_ancestor(t, s.snapshot(), root_id),
                         id::snapshot_tree_to_wrong_subvol,
                         "snapshot tree points to subvolume that does not point to snapshot in this tree:\n{}",
                         k.to_text(fs))? ||
            fsck_err_on!(t, s.snap(), id::snapshot_tree_to_snapshot_subvol,
                         "snapshot tree points to snapshot subvolume:\n{}", k.to_text(fs))?,
    };
    if !repair {
        return Ok(());
    }

    let Some(subvol) = bch_err_fn!(fs, tree_master_subvol(t, root_id))? else {
        return Ok(());   /* nothing to be done here */
    };

    let mut u = t.bkey_make_mut(iter, k, UpdateTriggerFlags::empty())?;
    u.v_mut().master_subvol = subvol.into();
    Ok(())
}

fn check_snapshot_trees(fs: &Fs) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    let mut iter = BtreeIter::new(&trans, c::btree_id::snapshot_trees, POS_MIN,
                                  BtreeIterFlags::PREFETCH);
    iter.for_each_commit(&trans, None, CommitFlags::NO_ENOSPC,
                         |t, iter, k| check_snapshot_tree(t, iter, k))
}

crate::recovery_pass!(bch2_check_snapshot_trees => check_snapshot_trees);

/* check_snapshots: */

/// The node being checked.
impl CowKey<'_, '_, '_, '_, c::Snapshot> {
    fn id(&self) -> u32 {
        self.pos().offset as u32
    }
}

/// Whether anything points at node @s, @id: a subvolume, its parent, or a
/// child. For a garbage state field: a referenced node must be live.
fn referenced(t: &TransAttempt<'_, '_>, s: &c::Snapshot, id: u32) -> Result<bool, BchError> {
    if s.subvol() != 0 &&
       subvolume::get(t, s.subvol(), false).found()?.is_some_and(|sv| sv.snapshot() == id) {
        return Ok(true);
    }

    if s.parent() != 0 &&
       lookup_node(t, s.parent())?.is_some_and(|p| points_at(&p.v, Role::Parent, id)) {
        return Ok(true);
    }

    for child in s.children() {
        if child != 0 &&
           lookup_node(t, child)?.is_some_and(|ch| points_at(&ch.v, Role::Child, id)) {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Recover the state field itself: stamp it from the legacy flags when
/// unset, decode a corrupted value back to the nearest codeword, and correct
/// a state left stale by a legacy flags-only tombstone.
fn check_state<'a, 't>(
    t: &TransAttempt<'a, 't>,
    n: &mut CowKey<'_, '_, 'a, 't, c::Snapshot>,
) -> Result<(), BchError> {
    let fs = t.fs();

    // Predates the state field, or was wiped: the legacy flags, which old kernels
    // dual-wrote, are authoritative - past the upgrade too, for keys never
    // migrated. Silent mid-upgrade, autofix after.
    if n.v().state().get() == 0 {
        let upgrading = fs.version_upgrade_complete() <
            c::bcachefs_metadata_version::per_dev_fragmentation_lru;
        if upgrading ||
           fsck_err!(t, id::snapshot_state_bad,
                     "snapshot state unset, recovering from legacy flags:\n{}", n.to_text(fs))? {
            let state = n.v().state_from_flags();
            n.v_mut(t)?.state = state.0.into();
        }
    }

    // The repairs below go by the state, so it has to be readable first.
    if n.v().state_field().is_none() {
        let raw = n.v().state().get();
        let (nearest, dist) = snapshot::state_nearest(raw);

        // Codewords are >= 14 bits apart: within 6 bits, the nearest is the
        // original. Within 2 is a bitflip, counted apart so failing hardware shows;
        // further than 6 can't be trusted.
        let fix = if dist <= 2 {
            fsck_err!(t, id::snapshot_state_bitflip,
                      "snapshot state {raw:#x} is a {dist}-bit flip of {nearest} - correcting:\n{}",
                      n.to_text(fs))?.then_some(nearest)
        } else if dist <= 6 {
            fsck_err!(t, id::snapshot_state_bad,
                      "snapshot state {raw:#x} is {dist} bits from {nearest} - correcting:\n{}",
                      n.to_text(fs))?.then_some(nearest)
        } else if referenced(t, n.v(), n.id())? {
            // Too far to decode, but if anything references it, it must be live
            fsck_err!(t, id::snapshot_state_bad,
                      "snapshot state {raw:#x} is garbage, but the node is referenced - marking live:\n{}",
                      n.to_text(fs))?.then_some(c::bch_snapshot_state::SNAPSHOT_STATE_live)
        } else {
            // An unreferenced garbage node we can't place: fail-stop.
            fsck_err_report!(fs, id::snapshot_state_bad,
                             "snapshot has invalid state {raw:#x} (nearest codeword {nearest} is {dist} bits away, node unreferenced):\n{}",
                             n.to_text(fs));
            return Err(fs.err(bch_errcode::BCH_ERR_fsck_repair_unimplemented));
        };

        if let Some(state) = fix {
            n.v_mut(t)?.set_state(state);
        }
    }

    // A valid state contradicting the legacy flags: only a legacy flags-only
    // writer does that, so the flags are the fresher write. But they're single
    // unprotected bits: trust them only on a node shaped like a legacy
    // bch2_snapshot_node_delete() tombstone - tree == 0, which nothing else ever
    // has. Left stale, the interior-deletion collector tombstones the node and the
    // edge checks fail-stop on its breadcrumb, wedging every mount (2026-07-20
    // field report; snapshot-inject/stale_state_tombstone).
    let v = n.v();
    if fsck_err_on!(t,
                    v.state().get() != c::bch_snapshot_state::SNAPSHOT_STATE_deleted.0 &&
                    v.deleted_obsolete() &&
                    v.tree == 0,
                    id::snapshot_state_stale_tombstone,
                    "snapshot spliced out by a legacy kernel (tombstone shape, legacy deleted flag)\n\
                     but the state field is stale at {} - correcting to deleted:\n{}",
                    StateName(v.state_field()), n.to_text(fs))? {
        n.v_mut(t)?.set_state(c::bch_snapshot_state::SNAPSHOT_STATE_deleted);
    }

    Ok(())
}

/// Data first: nothing we write deletes a node with keys still accounted to
/// it, so data means the state field is the lie. Settled here, every check
/// after - and every edge repair a parent runs against this node - sees a
/// state held up against the accounting.
///
/// deleted was spliced out of the tree, so reviving it undoes the splice;
/// no_keys is only ever marked, so setting it live is the whole repair.
fn check_has_data<'a, 't>(
    t: &TransAttempt<'a, 't>,
    n: &mut CowKey<'_, '_, 'a, 't, c::Snapshot>,
) -> Result<(), BchError> {
    let fs = t.fs();
    let state = n.v().state_field();

    if state != Some(c::bch_snapshot_state::SNAPSHOT_STATE_deleted) && state != Some(c::bch_snapshot_state::SNAPSHOT_STATE_no_keys) {
        return Ok(());
    }
    let no_keys = state == Some(c::bch_snapshot_state::SNAPSHOT_STATE_no_keys);

    let mut breakdown = Printbuf::new();
    let (keys, sectors) = fs.snapshots.accounting_totals(n.id(), Some(&mut breakdown))?;

    if !fsck_err_on!(t, keys != 0 || sectors != 0, id::snapshot_deleted_but_has_data,
                     "{} snapshot node has data accounted - {}:{}\n{}",
                     StateName(state),
                     if no_keys { "clearing state" } else { "undeleting" },
                     breakdown, n.to_text(fs))? {
        return Ok(());
    }

    if no_keys {
        n.v_mut(t)?.set_state(c::bch_snapshot_state::SNAPSHOT_STATE_live);
        Ok(())
    } else {
        undelete_owns_data(t, n.u(t)?)
    }
}

/// A non-live state, checked against the child snapshots and the
/// subvolume. True if the node is a settled tombstone: no further checking.
fn check_deleted<'a, 't>(
    t: &TransAttempt<'a, 't>,
    n: &mut CowKey<'_, '_, 'a, 't, c::Snapshot>,
) -> Result<bool, BchError> {
    let fs = t.fs();
    let id = n.id();

    if n.v().state().get() == c::bch_snapshot_state::SNAPSHOT_STATE_live.0 {
        return Ok(false);
    }

    // A non-live node can't have two live children pointing back at it: two
    // subtrees still depend on it as their common ancestor. Revive it - before
    // the deleted early-out, falling through to the edge, depth and tree checks.
    // One live child is legal: a no_keys node keeps its one descendant so
    // ancestry resolves. A child no longer naming us is a splice breadcrumb, not
    // counted.
    let mut nr_live_children = 0;
    for child in n.v().children() {
        if child != 0 &&
           lookup_node(t, child)?.is_some_and(|ch| ch.v.state_decoded() == Some(c::bch_snapshot_state::SNAPSHOT_STATE_live) &&
                                                      points_at(&ch.v, Role::Child, id)) {
            nr_live_children += 1;
        }
    }

    if fsck_err_on!(t, nr_live_children == 2, id::snapshot_deleted_has_live_children,
                    "snapshot marked {} but has two live children - reviving:\n{}",
                    StateName(n.v().state_field()), n.to_text(fs))? {
        n.v_mut(t)?.set_state(c::bch_snapshot_state::SNAPSHOT_STATE_live);
    }

    // Deletion tombstones the subvolume in the transaction that marks its
    // snapshot, so a non-live leaf whose subvolume is live and points back has a
    // bad state field - including one check_state() stamped from a corrupt legacy
    // flag. (A tombstoned subvolume reads as ENOENT: normal mid-deletion.)
    let v = n.v();
    if v.children()[0] == 0 && v.subvol() != 0 {
        let subvol_live = subvolume::get(t, v.subvol(), false).found()?.is_some_and(|sv|
            sv.state_decoded() == Some(c::bch_subvolume_state::SUBVOLUME_STATE_live) && sv.snapshot() == id);

        if subvol_live &&
           fsck_err!(t, id::snapshot_deleted_but_subvol_live,
                     "snapshot marked {} but its subvolume is live - reviving:\n{}",
                     StateName(v.state_field()), n.to_text(fs))? {
            n.v_mut(t)?.set_state(c::bch_snapshot_state::SNAPSHOT_STATE_live);
        }
    }

    Ok(n.v().state().get() == c::bch_snapshot_state::SNAPSHOT_STATE_deleted.0)
}

/* Parent <-> child edge checks and repair:
 *
 * An edge is repaired only if the surviving pointer and the two nodes' id
 * ordering, tree and depth all agree (those are checked before the
 * depth/skip autofixes rewrite them). If they don't, fail-stop: a wrong join
 * or split moves keys between subvolume visibilities. Never write "I don't
 * know" to disk: a node with a zeroed parent masquerades as a tree root and
 * gets consumed by the tree-pointer repair or the deletion machinery.
 * Repairs commit and restart, so decisions only see settled state. The
 * in-memory snapshot table serves as the reverse index (live nodes only: a
 * tombstone's child pointer is a splice breadcrumb, not a claim).
 */

/// A node's end of a parent/child edge - and so which of its pointers names
/// the other end: a parent's children, a child's parent.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Parent,
    Child,
}

impl Role {
    fn other(self) -> Self {
        match self {
            Role::Parent => Role::Child,
            Role::Child  => Role::Parent,
        }
    }

    /// What this end's pointer is called.
    fn ptr_name(self) -> &'static str {
        match self {
            Role::Parent => "child",
            Role::Child  => "parent",
        }
    }
}

/// Whether @s, as @role, points at @other.
fn points_at(s: &c::Snapshot, role: Role, other: u32) -> bool {
    match role {
        Role::Parent => s.children().contains(&other),
        Role::Child  => s.parent() == other,
    }
}

/// Point @s's @role pointer at @old to @new instead - 0 clears a child slot
/// - keeping the children in order.
fn set_ptr(s: &mut c::Snapshot, role: Role, old: u32, new: u32) {
    match role {
        Role::Child  => s.parent = new.into(),
        Role::Parent => {
            if let Some(child) = s.children.iter_mut().find(|c| (**c).get() == old) {
                *child = new.into();
            }
            if s.children()[0] < s.children()[1] {
                s.children.swap(0, 1);
            }
        }
    }
}

/// Whether @s, @id, as @role, and @o, @o_id, are shaped like the two ends of
/// one edge: the parent's id above the child's, the same tree, and the
/// child one deeper.
fn parent_child_consistent(s: &c::Snapshot, id: u32, role: Role,
                           o: &c::Snapshot, o_id: u32) -> bool {
    let ((pa, pa_id), (ch, ch_id)) = match role {
        Role::Parent => ((s, id), (o, o_id)),
        Role::Child  => ((o, o_id), (s, id)),
    };

    ch_id < pa_id && pa.tree == ch.tree && pa.depth().get().checked_add(1) == Some(ch.depth().get())
}

/// Whether node @n, @n_id, as @role, has a pointer slot that's empty, or
/// whose current target doesn't reciprocate - and if so, the value that
/// would be displaced. Empty slots first, so a stale pointer keeps its own
/// shot at repair when its node is visited.
fn ptr_available(t: &TransAttempt<'_, '_>, n: &c::Snapshot, n_id: u32, role: Role)
    -> Result<Option<u32>, BchError>
{
    let parent = [n.parent()];
    let children = n.children();
    let ptrs: &[u32] = match role {
        Role::Child  => &parent,
        Role::Parent => &children,
    };

    if ptrs.contains(&0) {
        return Ok(Some(0));
    }

    for &p in ptrs {
        if !lookup_node(t, p)?.is_some_and(|n| points_at(&n.v, role.other(), n_id)) {
            return Ok(Some(p));
        }
    }

    Ok(None)
}

/// Whether snapshot @id owns anything: any nonzero counter in any snapshotted
/// btree. Keys as well as sectors: a snapshot of only dirents or xattrs has no
/// sectors. In memory, so current without a write buffer flush.
fn has_accounting(fs: &Fs, id: u32) -> bool {
    crate::BTREE_IDS_KNOWN.iter()
        .filter(|&&btree| crate::BTREE_HAS_SNAPSHOTS_MASK & (1 << btree.0) != 0)
        .any(|&btree| {
            let acc = c::bch_acct_snapshot { id, btree: btree.0 };
            let mut v = [0u64; 3];
            accounting::mem_read(fs, &c::disk_accounting_pos::from_arm(acc), &mut v);
            v.iter().any(|&x| x != 0)
        })
}

/// Commit an edge repair, have the table rebuilt, and restart: decisions
/// only see settled state. The commit first - the restart would discard the
/// repair.
fn edge_repair_commit(t: &TransAttempt<'_, '_>) -> Result<(), BchError> {
    let fs = t.fs();
    t.commit(None, CommitFlags::NO_ENOSPC)?;
    fs.snapshots.set_need_table_rebuild();
    Err(t.restart(bch_errcode::BCH_ERR_transaction_restart_nested))
}

/// Put a node the accounting says is alive, @u, back into the tree. With two
/// children it was never spliced out: setting it live is the whole repair,
/// and node_undelete() rejects that shape. Otherwise it relinks through the
/// parent, which has to be live first.
fn undelete_owns_data<'t>(t: &TransAttempt<'_, 't>, u: &mut TransBkey<'_, 't, c::Snapshot>)
    -> Result<(), BchError>
{
    let s = u.v_mut();
    if s.children()[1] != 0 {
        s.set_state(c::bch_snapshot_state::SNAPSHOT_STATE_live);
        return Ok(());
    }

    undelete_ancestors(t, s.parent())?;
    snapshot::node_undelete(t, u)
}

/// The highest dead node from @id up: the one whose own parent is still in
/// the tree.
fn topmost_dead_ancestor(t: &TransAttempt<'_, '_>, mut id: u32) -> Result<Option<u32>, BchError> {
    let mut topmost = None;

    while id != 0 {
        // a missing parent is undelete's to report, against the node naming it
        let Some(s) = lookup_node(t, id)? else { break };
        if s.v.state_decoded() != Some(c::bch_snapshot_state::SNAPSHOT_STATE_deleted) {
            break;
        }

        topmost = Some(id);

        let parent = s.v.parent();
        if parent <= id {
            break;
        }
        id = parent;
    }

    Ok(topmost)
}

/// Undelete relinks through the parent, so undelete @id's dead ancestors first,
/// the topmost per commit - the chain is unbounded. Refusing would make a
/// repairable filesystem go emergency read-only. Nothing corroborates them;
/// they stay because the node below ends up live. Depth and skiplists are left
/// for snapshot_bad_depth/snapshot_bad_skiplist.
fn undelete_ancestors(t: &TransAttempt<'_, '_>, id: u32) -> Result<(), BchError> {
    let Some(topmost) = topmost_dead_ancestor(t, id)? else { return Ok(()) };

    let mut u = get_mut_node(t, topmost)?;
    undelete_owns_data(t, &mut u)?;
    edge_repair_commit(t)
}

/// Recreate missing child @id of @parent_id, as a leaf. Nothing we write
/// removes a node with keys still accounted to it, so it was destroyed, and
/// its keys are still reachable from every view below it. A leaf is what
/// @parent_id's pointer justifies; descendants, if any, come back through
/// their own parent pointers on a later pass. A subvolume claiming @id comes
/// back with it.
fn resurrect_child(t: &TransAttempt<'_, '_>, parent_id: u32, id: u32) -> Result<(), BchError> {
    let fs = t.fs();

    let Some(parent) = lookup_node(t, parent_id)? else {
        return Err(fs.err(bch_errcode::BCH_ERR_EINVAL_snapshot_edge_to_missing_node));
    };

    let mut n = t.bkey_alloc_typed::<c::Snapshot>()?;
    n.k_mut().p = pos(0, id as u64);

    let v = n.v_mut();
    v.parent    = parent_id.into();
    v.tree      = parent.v.tree;
    v.depth     = fs.snapshots.depth(parent_id).into();
    v.btime.lo  = fs.current_time().into();
    for skip in &mut v.skip {
        *skip = fs.snapshots.skiplist_get(parent_id).into();
    }
    v.skip.sort_unstable_by_key(|s| (*s).get());
    v.subvol    = subvol_claiming(t, id)?.unwrap_or(0).into();
    v.set_state(c::bch_snapshot_state::SNAPSHOT_STATE_live);

    fs.snapshots.table_make_room(id)?;
    t.insert(c::btree_id::snapshots, n, UpdateTriggerFlags::empty())
}

/// The nodes an edge repair is between, keys and all: which pointer is wrong
/// is only decidable across both, so the message shows both.
struct EdgeNodes<'n, 'f> {
    fs:       &'f Fs,
    node:     BkeySC<'n>,
    target:   Option<&'n BkeyI<c::Snapshot>>,
    claimant: Option<&'n BkeyI<c::Snapshot>>,
}

impl<'n, 'f> EdgeNodes<'n, 'f> {
    fn new(
        fs:       &'f Fs,
        node:     &'n CowKey<'_, '_, '_, '_, c::Snapshot>,
        target:   Option<&'n BkeyI<c::Snapshot>>,
        claimant: Option<&'n BkeyI<c::Snapshot>>,
    ) -> Self {
        EdgeNodes { fs, node: node.key(), target, claimant }
    }
}

impl fmt::Display for EdgeNodes<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "\nnode:     {}", self.node.to_text(self.fs))?;
        if let Some(t) = self.target {
            write!(f, "\ntarget:   {}", BkeySC::from(t.k_i()).to_text(self.fs))?;
        }
        if let Some(c) = self.claimant {
            write!(f, "\nclaimant: {}", BkeySC::from(c.k_i()).to_text(self.fs))?;
        }
        Ok(())
    }
}

/// Walking from deleted node @s past the deleted nodes beyond it, in the
/// direction @role's pointer goes: the nearest one that isn't deleted, 0 if
/// they all are - None if the chain is too long to be anything but damage.
fn past_deleted(t: &TransAttempt<'_, '_>, s: &c::Snapshot, role: Role)
    -> Result<Option<u32>, BchError>
{
    let next = |s: &c::Snapshot| match role {
        Role::Child  => s.parent(),
        Role::Parent => s.children()[0],
    };

    let mut id = next(s);
    for _ in 0..c::BTREE_MAX_DEPTH * 64 {
        if id == 0 {
            return Ok(Some(0));
        }
        match lookup_node(t, id)? {
            None => return Ok(Some(0)),
            Some(s) if s.v.state_decoded() != Some(c::bch_snapshot_state::SNAPSHOT_STATE_deleted) => return Ok(Some(id)),
            Some(s) => id = next(&s.v),
        }
    }
    Ok(None)   /* damaged chain, cycle? don't guess */
}

/// What to do about a broken edge, decided by edge_repair().
enum EdgeRepair {
    /// Undelete this node: it's deleted, but still owns data.
    Undelete(u32),
    /// Aim our pointer at this node instead - 0 to clear it.
    Retarget(u32),
    /// The target's pointer slot that should name us, empty or stale: aim it
    /// at us.
    Complete(u32),
    /// Recreate the missing child as a leaf.
    Resurrect,
}

/// Check the edge between node @n, as @role, and @other_id, which its
/// pointer names: a repair, if it needs one and fsck says to make it.
fn edge_repair(
    t:        &TransAttempt<'_, '_>,
    n:        &CowKey<'_, '_, '_, '_, c::Snapshot>,
    role:     Role,
    other_id: u32,
) -> Result<Option<EdgeRepair>, BchError> {
    let fs = t.fs();
    let id = n.id();
    let ptr = role.ptr_name();

    let other = lookup_node(t, other_id)?;

    // The live tree must be closed over nodes that aren't deleted: an edge into a
    // tombstone is left by an interrupted deletion, so rewrite ours past it - up
    // the tombstone's retained parents or down its retained children - to the
    // nearest node that isn't deleted. The tombstone itself is untouched.
    //
    // Only a tombstone without data. check_deleted() can't have ruled on it yet:
    // we walk down from POS_MAX and ids descend from the root, so a parent
    // reaches a deleted child's edge before the child is visited.
    if let Some(o) = other.as_ref().filter(|o| o.v.state_decoded() == Some(c::bch_snapshot_state::SNAPSHOT_STATE_deleted)) {
        // A deleted node with data has a lying state, and routing around it would
        // strand the keys: undelete it, and leave our edge to its own visit.
        let (keys, sectors) = fs.snapshots.accounting_totals(other_id, None)?;
        if keys != 0 || sectors != 0 {
            let fix = fsck_err!(t, id::snapshot_deleted_but_has_data,
                                "snapshot {id} {ptr} pointer {other_id} is deleted but has {keys} keys, \
                                 {sectors} sectors accounted - undeleting rather than splicing past it:{}",
                                EdgeNodes::new(fs, n, Some(o), None))?;
            return Ok(fix.then_some(EdgeRepair::Undelete(other_id)));
        }

        let Some(repl) = past_deleted(t, &o.v, role)? else { return Ok(None) };
        let fix = fsck_err!(t, id::snapshot_deleted_but_linked,
                            "snapshot {id} {ptr} pointer {other_id} is a deleted node - {} {repl}{}",
                            if repl != 0 { "re-linking past it to" } else { "clearing, dead in that direction:" },
                            EdgeNodes::new(fs, n, Some(o), None))?;
        return Ok(fix.then_some(EdgeRepair::Retarget(repl)));
    }

    if other.as_ref().is_some_and(|o| points_at(&o.v, role.other(), id)) {
        return Ok(None);
    }

    // Our pointer completes the edge if the target's slot toward us is empty
    // or holds a stale pointer - never a tombstone's, handled above.
    if let Some(o) = &other {
        if parent_child_consistent(n.v(), id, role, &o.v, other_id) {
            if let Some(old) = ptr_available(t, &o.v, other_id, role.other())? {
                if fsck_err!(t, id::snapshot_edge_bad,
                             "snapshot {id} {ptr} pointer {other_id} is not reciprocated, but is corroborated by\n\
                              tree and depth and the target's position ({old}) is unattested - completing the edge{}",
                             EdgeNodes::new(fs, n, Some(o), None))? {
                    return Ok(Some(EdgeRepair::Complete(old)));
                }
            }
        }
    }

    // Or another node claims the edge and tree and depth agree - re-aim ours:
    if let Some(repl) = fs.snapshots.table_find_edge(n.v(), id, role == Role::Child) {
        if let Some(r) = lookup_node(t, repl)? {
            if parent_child_consistent(n.v(), id, role, &r.v, repl) &&
               fsck_err!(t, id::snapshot_edge_bad,
                         "snapshot {id} {ptr} pointer {other_id} is broken (target {}), but node {repl} claims the\n\
                          edge, corroborated by tree and depth - repairing{}",
                         if other.is_some() { "does not reciprocate" } else { "does not exist" },
                         EdgeNodes::new(fs, n, other.as_ref(), Some(&r)))? {
                return Ok(Some(EdgeRepair::Retarget(repl)));
            }
        }
    }

    // A child pointer to a node that isn't there: keys accounted to it mean it
    // was destroyed, and recreating it keeps them reachable. With none, clear the
    // pointer - so a garbage id doesn't conjure a snapshot.
    if role == Role::Parent && other.is_none() {
        let [c0, c1] = n.v().children();
        let sibling = if c0 == other_id { c1 } else { c0 };

        if has_accounting(fs, other_id) {
            let fix = fsck_err!(t, id::snapshot_child_missing_but_accounted,
                                "snapshot {id} child pointer {other_id} does not exist, but keys are\n\
                                 accounted to {other_id} - recreating it as a leaf so they stay reachable{}",
                                EdgeNodes::new(fs, n, None, None))?;
            return Ok(fix.then_some(EdgeRepair::Resurrect));
        }

        if sibling != 0 &&
           fsck_err!(t, id::snapshot_edge_bad,
                     "snapshot {id} child pointer {other_id} does not exist: nothing claims {id} as\n\
                      parent and nothing is accounted to it - clearing{}",
                     EdgeNodes::new(fs, n, None, None))? {
            return Ok(Some(EdgeRepair::Retarget(0)));
        }
    }

    // One stomped field leaves the other side's pointer for the repairs above, so
    // this is multiple corruptions or a destroyed node: report and stop.
    bch_err!(fs, "snapshot topology damage is beyond single-corruption repair:\n\
                  node {id}'s {ptr} pointer names {other_id}, which {}\n\
                  no other node passes the parent/child consistency checks for this edge{}\n\
                  not repairing: run fsck; if damage is extensive, reconstruct_snapshots rebuilds topology from key evidence",
             if other.is_some() {
                 "exists but does not point back, and tree/depth do not identify them as parent and child"
             } else {
                 "does not exist"
             },
             EdgeNodes::new(fs, n, other.as_ref(), None));

    Err(fs.err(match (&other, role) {
        (None, _)          => bch_errcode::BCH_ERR_EINVAL_snapshot_edge_to_missing_node,
        (_, Role::Child)   => bch_errcode::BCH_ERR_EINVAL_snapshot_parent_missing_child_ptr,
        (_, Role::Parent)  => bch_errcode::BCH_ERR_EINVAL_snapshot_child_bad_parent,
    }))
}

/// Check, and repair, the edge between node @n, as @role, and @other_id,
/// which its pointer names.
fn check_edge<'a, 't>(
    t:        &TransAttempt<'a, 't>,
    n:        &mut CowKey<'_, '_, 'a, 't, c::Snapshot>,
    role:     Role,
    other_id: u32,
) -> Result<(), BchError> {
    let Some(repair) = edge_repair(t, n, role, other_id)? else { return Ok(()) };

    match repair {
        EdgeRepair::Undelete(id) =>
            undelete_owns_data(t, &mut get_mut_node(t, id)?)?,
        EdgeRepair::Retarget(new) =>
            set_ptr(n.v_mut(t)?, role, other_id, new),
        EdgeRepair::Complete(old) =>
            set_ptr(get_mut_node(t, other_id)?.v_mut(), role.other(), old, n.id()),
        EdgeRepair::Resurrect =>
            resurrect_child(t, n.id(), other_id)?,
    }
    edge_repair_commit(t)
}

/// Whether @id's tree pointer, @tree, names a tree that exists and whose
/// root @id descends from.
fn tree_ptr_good(t: &TransAttempt<'_, '_>, id: u32, tree: u32) -> Result<bool, BchError> {
    let Some(st) = snapshot::tree_lookup(t, tree).found()? else { return Ok(false) };
    Ok(t.fs().snapshots.is_ancestor_early(id, st.root_snapshot.get()))
}

/// Snapshot node @n's tree pointer was wrong: make sure its root's is right -
/// creating a tree if there isn't one for it - and point @n at that.
fn tree_ptr_repair<'a, 't>(
    t: &TransAttempt<'a, 't>,
    n: &mut CowKey<'_, '_, 'a, 't, c::Snapshot>,
) -> Result<(), BchError> {
    let fs = t.fs();
    let id = n.id();
    let root_id = fs.snapshots.root(id);

    let mut tree_id = if root_id == id {
        n.v().tree()
    } else {
        snapshot::lookup_key(t, root_id)?.v.tree()
    };

    let tree = snapshot::tree_lookup(t, tree_id).found()?;
    if tree.is_none_or(|st| st.root_snapshot.get() != root_id) {
        let mut new = snapshot::tree_create(t)?;
        let v = new.v_mut();
        v.master_subvol = fs.snapshots.oldest_subvol(root_id).unwrap_or(0).into();
        v.root_snapshot = root_id.into();
        tree_id = new.k().p.offset as u32;

        if root_id != id {
            get_mut_node(t, root_id)?.v_mut().tree = tree_id.into();
        }
    }

    if n.v().tree() != tree_id {
        n.v_mut(t)?.tree = tree_id.into();
    }
    Ok(())
}

/// Depth is derived from the parent - and the parent was visited first.
fn check_depth<'a, 't>(
    t: &TransAttempt<'a, 't>,
    n: &mut CowKey<'_, '_, 'a, 't, c::Snapshot>,
) -> Result<(), BchError> {
    let fs = t.fs();
    let parent_id = n.v().parent();

    // No parent to derive depth from, and 0 would flatten an interior node to a
    // root. check_edge() reported the pointer; leave depth until it's fixed.
    // Likewise a parent at the maximum depth: that's damage, reported at the
    // parent, and still there if it wasn't repaired (fsck -n).
    let real_depth = if parent_id == 0 {
        0
    } else {
        let Some(parent) = lookup_node(t, parent_id)? else { return Ok(()) };
        let Some(depth) = parent.v.depth().get().checked_add(1) else { return Ok(()) };
        depth
    };

    if fsck_err_on!(t, n.v().depth() != real_depth, id::snapshot_bad_depth,
                    "snapshot with incorrect depth field, should be {real_depth}:\n{}",
                    n.to_text(fs))? {
        n.v_mut(t)?.depth = real_depth.into();
    }
    Ok(())
}

/// Every skiplist entry is an ancestor - none, for a root.
fn check_skiplists<'a, 't>(
    t: &TransAttempt<'a, 't>,
    n: &mut CowKey<'_, '_, 'a, 't, c::Snapshot>,
) -> Result<(), BchError> {
    let fs = t.fs();
    let id = n.id();

    for i in 0..n.v().skip.len() {
        let parent_id = n.v().parent();
        let skip = (n.v().skip[i]).get();

        let bad = if parent_id == 0 { skip != 0 } else { !fs.snapshots.is_ancestor_early(id, skip) };
        if !bad {
            continue;
        }

        let mut msg = Printbuf::new();
        write!(msg, "snapshot with bad skiplist pointer {skip}:\n{}\n", n.to_text(fs));
        if skip != 0 {
            let mut iter = BtreeIter::new(t, c::btree_id::snapshots, pos(0, skip as u64),
                                          BtreeIterFlags::empty());
            let skip_k = iter.peek_slot(t)?.expect("a slot always has a key");
            write!(msg, "points to\n  {}\n", skip_k.to_text(fs));
        }

        if fsck_err!(t, id::snapshot_bad_skiplist, "{msg}")? {
            n.v_mut(t)?.skip[i] = fs.snapshots.skiplist_get(parent_id).into();
        }
    }

    // Kept sorted: is_ancestor() tries the highest first.
    if n.has_update() {
        n.v_mut(t)?.skip.sort_unstable_by_key(|s| (*s).get());
    }
    Ok(())
}

/// The subvolume backref, against the subvolume it names.
fn check_to_subvol<'a, 't>(
    t: &TransAttempt<'a, 't>,
    n: &mut CowKey<'_, '_, 'a, 't, c::Snapshot>,
) -> Result<(), BchError> {
    let fs = t.fs();
    let id = n.id();

    let state = n.v().state_field();
    let snap_deleting = state == Some(c::bch_snapshot_state::SNAPSHOT_STATE_will_delete);
    let should_have_subvol = n.v().children()[0] == 0 &&
        (state == Some(c::bch_snapshot_state::SNAPSHOT_STATE_live) || snap_deleting);

    if n.v().subvol() != 0 {
        let subvol_id = n.v().subvol();

        // A raw read: subvolume::get() reports deleted subvolumes as ENOENT,
        // and the message should show what's actually there.
        let mut iter = BtreeIter::new(t, c::btree_id::subvolumes, pos(0, subvol_id as u64),
                                      BtreeIterFlags::empty());
        let subvol_k = iter.peek_slot(t)?.expect("a slot always has a key");
        let subvol = subvolume::val(subvol_k);

        // Only here can a missing subvolume be rebuilt: a snapshot with a subvol
        // backref is a leaf, which reconstruct_subvol() needs and an inode's or
        // dirent's snapshot can't promise. Not while the snapshot is deleting: that's
        // a tombstoned deletion in flight.
        if subvol.is_none() && !snap_deleting {
            match check::reconstruct_subvol_root(t, id, subvol_id, None) {
                Ok(root) => return check::reconstruct_subvol(t, id, subvol_id, root),
                // couldn't find a root inode for it - fall through and report
                Err(e) if e.matches(bch_errcode::BCH_ERR_fsck_repair_unimplemented) => {}
                Err(e) => return Err(e),
            }
        }

        // Wrong backref, or not rebuildable: the repair belongs to the pass after
        // check_subvols. Report only - an error would break mounts mid-deletion.
        let subvol = match subvol {
            Some(s) if s.snapshot() == id => s,
            Some(_) => {
                fsck_err_report!(fs, id::snapshot_subvol_backref_wrong,
                                 "snapshot's subvolume doesn't point back at it:\n{}\n{}",
                                 n.to_text(fs), subvol_k.to_text(fs));
                return Ok(());
            }
            None => {
                fsck_err_report!(fs, id::snapshot_subvol_backref_wrong,
                                 "snapshot points to missing subvolume {subvol_id}:\n{}",
                                 n.to_text(fs));
                return Ok(());
            }
        };

        // Deletion couples one bit on each side: will_delete iff the subvolume is
        // tombstoned. The subvolume decides - the reverse would have to guess live or
        // unlinked.
        let subvol_deleted = subvol.state_decoded() == Some(c::bch_subvolume_state::SUBVOLUME_STATE_deleted);
        if fsck_err_on!(t, snap_deleting != subvol_deleted, id::snapshot_subvol_state_mismatch,
                        "snapshot {} but its subvolume is {}:\n{}\n{}",
                        if snap_deleting { "will_delete" } else { "live" },
                        if subvol_deleted { "deleted" } else { "not deleted" },
                        n.to_text(fs), subvol_k.to_text(fs))? {
            n.v_mut(t)?.set_state(if subvol_deleted {
                c::bch_snapshot_state::SNAPSHOT_STATE_will_delete
            } else {
                c::bch_snapshot_state::SNAPSHOT_STATE_live
            });
        }
    } else if should_have_subvol && state == Some(c::bch_snapshot_state::SNAPSHOT_STATE_live) {
        // No backref: if a subvolume points here it was wiped - restore it (a second
        // claimant still hits check_subvols' fail-stop). No claimant is an orphan
        // leaf, unrepaired.
        if let Some(subvol) = subvol_claiming(t, id)? {
            if fsck_err!(t, id::snapshot_subvol_backref_wrong,
                         "snapshot leaf missing subvol backref, subvolume {subvol} points at it - restoring:\n{}",
                         n.to_text(fs))? {
                let v = n.v_mut(t)?;
                v.subvol = subvol.into();
                v.set_subvol_obsolete(true);
            }
        }
    }

    if fsck_err_on!(t, n.v().subvol() != 0 && !should_have_subvol, id::snapshot_should_not_have_subvol,
                    "snapshot should not point to subvol:\n{}", n.to_text(fs))? {
        if n.v().children()[0] != 0 {
            return Err(fs.err(bch_errcode::BCH_ERR_fsck_repair_unimplemented));
        }

        // XXX: DANGEROUS
        n.v_mut(t)?.subvol = 0.into();
    }

    // Live nodes only: set_state() clears SUBVOL_OBSOLETE for every non-live
    // state, backref or not, so old kernels don't read a dying snapshot as a live
    // subvolume leaf.
    let v = n.v();
    if v.state().get() == c::bch_snapshot_state::SNAPSHOT_STATE_live.0 && v.subvol_obsolete() != (v.subvol() != 0) {
        let mut msg = Printbuf::new();
        write!(msg, "snapshot node {id} has wrong subvol flag:\n{}", n.to_text(fs));

        if v.subvol() != 0 {
            let mut iter = BtreeIter::new(t, c::btree_id::subvolumes, pos(0, v.subvol() as u64),
                                          BtreeIterFlags::empty());
            let subvol_k = iter.peek_slot(t)?.expect("a slot always has a key");
            write!(msg, "\n{}", subvol_k.to_text(fs));
        }

        if fsck_err!(t, id::snapshot_subvol_flag_wrong, "{msg}")? {
            let has_subvol = n.v().subvol() != 0;
            n.v_mut(t)?.set_subvol_obsolete(has_subvol);
        }
    }

    Ok(())
}

fn check_snapshot<'a, 't>(
    t:    &TransAttempt<'a, 't>,
    iter: &BtreeIter<'t>,
    k:    BkeySC<'_>,
) -> Result<(), BchError> {
    let fs = t.fs();

    let Some(mut n) = CowKey::<c::Snapshot>::new(iter, k) else { return Ok(()) };

    check_state(t, &mut n)?;
    check_has_data(t, &mut n)?;

    if check_deleted(t, &mut n)? {
        return Ok(());
    }

    let parent = n.v().parent();
    if parent != 0 {
        check_edge(t, &mut n, Role::Child, parent)?;
    }
    for child in n.v().children() {
        if child != 0 {
            check_edge(t, &mut n, Role::Parent, child)?;
        }
    }

    if !tree_ptr_good(t, n.id(), n.v().tree())? &&
       fsck_err!(t, id::snapshot_to_bad_snapshot_tree,
                 "snapshot points to missing/incorrect tree:\n{}", n.to_text(fs))? {
        tree_ptr_repair(t, &mut n)?;
    }

    check_depth(t, &mut n)?;
    check_skiplists(t, &mut n)?;
    check_to_subvol(t, &mut n)
}

fn check_snapshots_trans(trans: &BtreeTrans<'_>) -> Result<(), BchError> {
    // Backwards: fixing a node's depth needs its parent's to be right already.
    let mut iter = BtreeIter::new(trans, c::btree_id::snapshots, POS_MAX, BtreeIterFlags::PREFETCH);
    iter.for_each_reverse_commit(trans, POS_MIN, None, CommitFlags::NO_ENOSPC,
                                 |t, iter, k| check_snapshot(t, iter, k))?;

    snapshot::table_rebuild_if_needed(trans)
}

/// check_snapshots, in @trans: for snapshot deletion, which runs it when it
/// finds the tree inconsistent.
///
/// # Safety
/// @trans is a live transaction, with no attempt in progress.
#[c_export]
pub unsafe extern "C" fn bch2_check_snapshots_trans(trans: *mut c::btree_trans) -> core::ffi::c_int {
    let trans = unsafe { BtreeTrans::borrow_raw(trans) };

    crate::errcode::ret_to_c(check_snapshots_trans(&trans))
}

fn check_snapshots(fs: &Fs) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    check_snapshots_trans(&trans)?;

    // A clean pass lets check_key_has_snapshot trust the in-memory table
    if !fs.flag(c::bch_fs_flags::BCH_FS_error) {
        passes::set_btree_clean(fs, c::btree_id::snapshots);
    }
    Ok(())
}

crate::recovery_pass!(bch2_check_snapshots => check_snapshots);

/* reconstruct_snapshots: */

/// Space separated.
struct IdList<'l>(&'l [u32]);

impl fmt::Display for IdList<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, id) in self.0.iter().enumerate() {
            if i != 0 {
                f.write_str(" ")?;
            }
            write!(f, "{id}")?;
        }
        Ok(())
    }
}

/// Which snapshot ids belong to the same tree, from the keys: every version
/// of one inode is in one tree.
struct TreeReconstruct {
    btree:   c::btree_id,
    cur_pos: c::bpos,
    /// The current inode's snapshot ids.
    cur_ids: KVVec<u32>,
    /// The trees so far; merged-away ones are left empty.
    trees:   KVVec<KVVec<u32>>,
}

impl TreeReconstruct {
    /// Whether @pos is the current inode.
    fn same_inode(&self, pos: c::bpos) -> bool {
        if self.btree == c::btree_id::inodes {
            self.cur_pos.offset == pos.offset
        } else {
            self.cur_pos.inode == pos.inode
        }
    }

    /// Done with the current inode: its ids and every tree they overlap
    /// become one tree.
    fn next(&mut self) -> Result<(), BchError> {
        if self.cur_ids.is_empty() {
            return Ok(());
        }

        let mut tree = core::mem::replace(&mut self.cur_ids, KVVec::new());
        for t in self.trees.iter_mut() {
            if t.iter().any(|id| tree.contains(id)) {
                for &id in t.iter() {
                    add_nodup(&mut tree, id)?;
                }
                t.clear();
            }
        }

        self.trees.push(tree, GFP_KERNEL)?;
        Ok(())
    }

    fn add(&mut self, pos: c::bpos) -> Result<(), BchError> {
        if !self.same_inode(pos) {
            self.next()?;
        }
        self.cur_pos = pos;
        add_nodup(&mut self.cur_ids, pos.snapshot)
    }
}

fn add_nodup(ids: &mut KVVec<u32>, id: u32) -> Result<(), BchError> {
    if !ids.contains(&id) {
        ids.push(id, GFP_KERNEL)?;
    }
    Ok(())
}

/// Recreate missing node @id, a tree to itself: with a tree key, if there
/// isn't one with it as the root.
fn recreate_node(t: &TransAttempt<'_, '_>, id: u32) -> Result<(), BchError> {
    let fs = t.fs();

    let tree_id = find_key(t, c::btree_id::snapshot_trees, |k| {
        k.arm::<c::SnapshotTree>().map(|k| k.v()).is_some_and(|st| st.root_snapshot.get() == id)
    })?;

    let tree_id = match tree_id {
        Some(tree_id) => tree_id,
        None => {
            let mut tree = snapshot::tree_create(t)?;
            tree.v_mut().root_snapshot = id.into();
            tree.k().p.offset as u32
        }
    };

    let mut n = t.bkey_alloc_typed::<c::Snapshot>()?;
    n.k_mut().p = pos(0, id as u64);

    let v = n.v_mut();
    v.tree     = tree_id.into();
    v.btime.lo = fs.current_time().into();
    v.subvol   = subvol_claiming(t, id)?.unwrap_or(0).into();
    v.set_state(c::bch_snapshot_state::SNAPSHOT_STATE_live);

    fs.snapshots.table_make_room(id)?;
    t.insert(c::btree_id::snapshots, n, UpdateTriggerFlags::empty())
}

/// Recreate node @id of @tree if it's missing - only if it's a tree to
/// itself: one we can't place.
fn reconstruct_node(t: &TransAttempt<'_, '_>, id: u32, tree: &[u32]) -> Result<(), BchError> {
    let fs = t.fs();

    if fsck_err_on!(t, fs.snapshots.id_state(id) == IdState::SNAPSHOT_ID_empty,
                    id::snapshot_node_missing,
                    "snapshot node {id} from tree {} missing, recreate?", IdList(tree))? {
        if tree.len() > 1 {
            bch_err!(fs, "cannot reconstruct snapshot trees with multiple nodes");
            return Err(fs.err(bch_errcode::BCH_ERR_fsck_repair_unimplemented));
        }

        return recreate_node(t, id);
    }

    Ok(())
}

fn reconstruct_snapshots(fs: &Fs) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);

    let mut btrees: KVVec<c::btree_id> = KVVec::new();
    for &btree in crate::BTREE_IDS_KNOWN {
        if crate::BTREE_HAS_SNAPSHOTS_MASK & (1 << btree.0) != 0 {
            btrees.push(btree, GFP_KERNEL)?;
        }
    }
    let progress = Progress::recovery(fs, c"bch2_reconstruct_snapshots", &btrees, &[]);

    let mut r = TreeReconstruct {
        btree:   c::btree_id::extents,
        cur_pos: POS_MIN,
        cur_ids: KVVec::new(),
        trees:   KVVec::new(),
    };

    for &btree in btrees.iter() {
        r.btree = btree;

        let mut iter = BtreeIter::new(&trans, btree, POS_MIN,
                                      BtreeIterFlags::ALL_SNAPSHOTS | BtreeIterFlags::PREFETCH);
        iter.for_each_attempt(&trans, |t, iter, k| {
            progress.update(t, iter)?;
            r.add(k.k.p)
        })?;

        r.next()?;
    }

    for tree in r.trees.iter() {
        for &id in tree.iter() {
            commit_do(&trans, None, CommitFlags::NO_ENOSPC, |t| reconstruct_node(t, id, tree))?;
        }
    }

    Ok(())
}

crate::recovery_pass!(bch2_reconstruct_snapshots => reconstruct_snapshots);

/* check_key_has_snapshot - per-key repair, also called at runtime: */

/// A key moved from a deleted snapshot to its live descendant @snapshot needs
/// an inode there, or the next deletion strands it again: copy one down if
/// it's only inherited. A missing inode is left for full fsck.
fn key_has_inode_in_snapshot(
    t:        &TransAttempt<'_, '_>,
    btree:    c::btree_id,
    inum:     u64,
    snapshot: u32,
) -> Result<(), BchError> {
    match btree {
        c::btree_id::extents | c::btree_id::dirents | c::btree_id::xattrs => {}
        _ => return Ok(()),
    }

    let Some(mut inode) = inode::find_by_inum_snapshot(t, inum, snapshot,
                                                       BtreeIterFlags::empty()).found()? else {
        return Ok(());
    };
    if inode.bi_snapshot == snapshot {
        return Ok(());
    }

    inode.bi_snapshot = snapshot;
    inode::fsck_write(t, &mut inode)
}

/// Key @k is in a snapshot that's missing or dead: repair it - deleted, or
/// moved to a live descendant - true if so, for the caller to skip it. With
/// no @iter - the promote path - it can't be repaired, and is just skipped.
pub(crate) fn check_key_has_snapshot(
    t:    &TransAttempt<'_, '_>,
    iter: Option<&mut c::btree_iter>,
    k:    BkeySC<'_>,
) -> Result<bool, BchError> {
    let fs = t.fs();
    let state = fs.snapshots.id_state(k.k.p.snapshot);

    let Some(iter) = iter else { return Ok(true) };

    if state == IdState::SNAPSHOT_ID_live {
        return Ok(false);
    }

    let mut buf = Printbuf::new();
    let mut ret = Ok(());
    let mut keep = |r: Result<(), BchError>| if r.is_err() { ret = r };

    // The deletion that stranded this key almost certainly stranded others across
    // the content btrees: schedule their passes now, before copygc or reconcile
    // trips over them. Only a rewind returns an error, deferring this key.
    // skip_if_complete: if they ran and the key's still here they can't fix it,
    // and rescheduling would force fsck on every mount.
    for pass in [BCH_RECOVERY_PASS_check_inodes, BCH_RECOVERY_PASS_check_extents,
                 BCH_RECOVERY_PASS_check_dirents, BCH_RECOVERY_PASS_check_xattrs] {
        keep(passes::run_explicit(fs, &mut buf, pass,
                                  c::bch_run_recovery_pass_flags::RUN_RECOVERY_PASS_skip_if_complete));
    }

    // A key in a deleted snapshot means it has data, and check_snapshot()
    // undeletes a node with data: the state is the lie. We're seeing the key, so
    // that hasn't happened yet - require check_snapshots before deleting or
    // migrating it, both irreversible. require() rewinds if we're past it; if it
    // ran and left the node deleted, it returns Ok and the repair proceeds.
    if state == IdState::SNAPSHOT_ID_deleted {
        keep(passes::require(fs, &mut buf, BCH_RECOVERY_PASS_check_snapshots));
    }

    // Both repairs act on the in-memory table: trust it only if the snapshots and
    // subvolumes btrees were checked clean and not changed since - not just
    // checked this mount. Otherwise schedule the passes and defer.
    if !passes::btree_is_clean(fs, c::btree_id::snapshots) ||
       !passes::btree_is_clean(fs, c::btree_id::subvolumes) {
        keep(passes::require(fs, &mut buf, BCH_RECOVERY_PASS_check_snapshots));
        keep(passes::require(fs, &mut buf, BCH_RECOVERY_PASS_check_subvols));
    }

    ret?;

    write!(buf, "{} {}", iter.btree_id(), k.to_text(fs));

    if state == IdState::SNAPSHOT_ID_deleted {
        // At runtime - data moves, bch2_data_update_init() - not in fsck, where
        // check_allocations rebuilds the accounting first and check_snapshots
        // undeletes a node with keys (fsck-inject test_deleted_interior_has_data).
        //
        // No live descendant: nothing can see the key, so delete it. A live
        // descendant still sees it by inheritance: migrate it there - deletion keeps a
        // child pointer for live_descendent() to find. An error finding it, on a
        // table just checked clean, is real damage: report it, keep the key.
        match fs.snapshots.live_descendent(k.k.p.snapshot)? {
            None => if inode_fsck_err!(t, k.k.p, id::bkey_in_deleted_snapshot,
                                       "key in deleted snapshot {buf}, delete?")? {
                t.delete_at_raw(iter, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
                return Ok(true);
            },
            Some(live_child) => if fsck_err!(t, id::bkey_in_deleted_interior_snapshot,
                                             "key in deleted interior snapshot {buf}, migrating to live descendant {live_child}")? {
                let (btree, inum) = (iter.btree_id(), k.k.p.inode);
                snapshot::delete_dead_key(t, iter, k, live_child)?;
                key_has_inode_in_snapshot(t, btree, inum, live_child)?;
                return Ok(true);
            },
        }
    } else if inode_fsck_err!(t, k.k.p, id::bkey_in_missing_snapshot,
                              "key in missing snapshot {buf}, delete?")? {
        t.delete_at_raw(iter, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
        return Ok(true);
    }

    Ok(false)
}

/// check_key_has_snapshot(), for C's bch2_check_key_has_snapshot(): 1 if @k
/// was repaired or can't be, for the caller to skip it.
///
/// # Safety
/// @iter NULL - the promote path - or the iterator @k was read from.
#[c_export]
pub unsafe extern "C" fn __bch2_check_key_has_snapshot(
    trans: &crate::util::ffi::Opaque<c::btree_trans>,
    iter:  Option<&mut c::btree_iter>,
    k:     BkeySC<'_>,
) -> core::ffi::c_int {
    match check_key_has_snapshot(&unsafe { BtreeTrans::from_c(trans).attempt_in_progress() }, iter, k) {
        Ok(handled) => handled as core::ffi::c_int,
        Err(e)      => -e.raw(),
    }
}
