// SPDX-License-Identifier: GPL-2.0

//! Snapshot fsck: the passes over the snapshots and snapshot_trees btrees.
//!
//! check_snapshot_trees: every tree key names a live root and master subvol.
//!
//! check_snapshots: per node - check_state() recovers the state field
//! itself; check_has_data() and check_deleted() hold a non-live state up
//! against the accounting, the child snapshots and the subvolume; then the
//! topology checks (edges, tree pointer, depth, skiplists, subvol backref).
//!
//! reconstruct_snapshots: rebuild missing snapshot nodes from the keys that
//! reference them.
//!
//! check_key_has_snapshot: per-key repair for keys whose snapshot node is
//! missing or dead; also called from runtime paths, through C's
//! bch2_check_key_has_snapshot().
//!
//! Repair philosophy: enumerate which writers can produce a state before
//! repairing it. Unconstructible combinations are rejected at commit
//! (bch2_snapshot_validate()); real damage is repaired toward the side the
//! rest of the metadata agrees with - child snapshots, the subvolume
//! (deletion tombstones it in the same transaction that condemns its
//! snapshot), the per-snapshot accounting (nothing we write deletes a node
//! with data). Ambiguity fail-stops rather than guessing.
//!
//! The node being checked is a Node: one copy of its value, read by every
//! check and written by every repair - the first repair makes it the node's
//! queued update. The C kept a copy for reading and a mutable key, synced by
//! hand, and some repairs made their own mutable copy from the original key:
//! a tree pointer repair followed by a depth, skiplist or subvolume flag
//! repair on the same node lost the tree pointer repair, and the accounting
//! check dropped what check_state() had queued.
//!
//! Changes from the C, besides that:
//!
//! - reconstruct_snapshots dropped errors growing its id lists; it returns
//!   them now. And it merged an inode's ids into the first tree they
//!   overlapped: ids overlapping two trees join them all into one.
//!
//! - check_key_has_snapshot only offered a repair it could also ignore
//!   after a failed attempt to schedule passes - which returned first: its
//!   errors are plain fixable fsck errors.

use crate::accounting::{self, DiskAccountingKind};
use crate::btree::bkey::{pos, BkeySC, BkeySCToText, POS_MAX, POS_MIN};
use crate::btree::iter::{
    commit_do, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, TransAttempt, TransBkey,
    TransResult, TransRet, UpdateTriggerFlags,
};
use crate::c;
use crate::c::bch_recovery_pass::*;
use crate::c::bch_snapshot_state::*;
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
use core::marker::PhantomData;
use core::mem::size_of;
use core::ops::ControlFlow;

/// Snapshot node @id's key, None if there's no such node.
///
/// fsck has no right to assume a node exists, so absence is a value here,
/// not an error - an error can be returned by mistake, a value can't: the C
/// laundered ENOENT back into a boolean at every call site, and once returned
/// the one it had just decided was benign, taking recovery emergency
/// read-only over a dangling child pointer. And the key comes back whole:
/// the topology checks report on nodes they didn't start from, and a field
/// report is whatever the message printed.
fn lookup_node(trans: &BtreeTrans<'_>, id: u32) -> Result<Option<c::bkey_i_snapshot>, BchError> {
    snapshot::lookup_key(trans, id).found()
}

/// Snapshot node @id's key, as a mutable copy queued for the commit.
fn get_mut_node<'a, 't>(t: &TransAttempt<'a, 't>, id: u32) -> Result<TransBkey<'a, 't>, BchError> {
    t.bkey_get_mut(c::btree_id::snapshots, pos(0, id as u64), UpdateTriggerFlags::empty(),
                   c::bch_bkey_type::KEY_TYPE_snapshot, size_of::<c::bkey_i_snapshot>())
}

fn snapshot_mut<'k>(k: &'k mut TransBkey<'_, '_>) -> &'k mut c::bch_snapshot {
    k.k_i_mut().as_mut_snapshot().expect("a snapshot key")
}

/// The id - offset - of the first key in @btree @pred accepts: for the
/// btrees keyed by id, subvolumes and snapshot trees.
fn find_key(
    trans:    &BtreeTrans<'_>,
    btree:    c::btree_id,
    mut pred: impl FnMut(BkeySC<'_>) -> bool,
) -> Result<Option<u32>, BchError> {
    let mut found = None;
    let mut iter = BtreeIter::new(trans, btree, POS_MIN, BtreeIterFlags::empty());
    iter.for_each_norestart(|_, k| {
        if pred(k) {
            found = Some(k.k.p.offset as u32);
            return Ok(ControlFlow::Break(()));
        }
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(found)
}

/// A subvolume claiming snapshot @id - to restore a wiped backref.
fn subvol_claiming(trans: &BtreeTrans<'_>, id: u32) -> Result<Option<u32>, BchError> {
    find_key(trans, c::btree_id::subvolumes,
             |k| subvolume::val(k).is_some_and(|s| s.snapshot() == id))
}

/// A state field, by name - "(invalid state)" if it isn't one: as
/// bch2_snapshot_state_str().
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
    let trans = t.trans();
    let fs = trans.fs();

    let found = find_key(trans, c::btree_id::subvolumes, |k| {
        subvolume::val(k).is_some_and(|s| snapshot::is_ancestor(trans, s.snapshot(), root) && !s.snap())
    })?;
    if found.is_some() {
        return Ok(found);
    }

    let Some(oldest) = snapshot::oldest_subvol(fs, root) else { return Ok(None) };
    let Some(mut u) = t.bkey_get_mut(c::btree_id::subvolumes, pos(0, oldest as u64),
                                     UpdateTriggerFlags::empty(),
                                     c::bch_bkey_type::KEY_TYPE_subvolume,
                                     size_of::<c::bkey_i_subvolume>()).found()? else {
        return Ok(None);
    };
    u.k_i_mut().as_mut_subvolume().expect("a subvolume key").set_snap(false);
    Ok(Some(oldest))
}

/// Make sure tree key @k points to the root of a snapshot tree and that
/// snapshot entry points back to it, or delete it. And make sure it points to
/// a non-snapshot subvolume in the tree, or correct it to one.
fn check_snapshot_tree<'a, 't>(
    t:    TransAttempt<'a, 't>,
    iter: &mut BtreeIter<'t>,
    k:    BkeySC<'_>,
) -> TransRet<'a, 't> {
    let trans = t.trans();
    let fs = trans.fs();

    let Some(st) = k.as_snapshot_tree() else { return Ok(t) };
    let root_id = u32::from_le(st.root_snapshot);

    let root = match snapshot::lookup_key(trans, root_id) {
        Err(e) if !e.matches(c::ENOENT) => return Err(e.into()),
        r => r,
    };

    let bad = match &root {
        Err(_) => true,
        Ok(r)  => root_id != snapshot::root(fs, root_id) || k.k.p.offset as u32 != r.v.tree(),
    };
    if bad {
        let mut found = Printbuf::new();
        match &root {
            Err(e) => write!(found, "({})", e.msg()),
            Ok(r)  => write!(found, "{}", BkeySC::from(r.k_i()).to_text(fs)),
        }
        if fsck_err!(&t, id::snapshot_tree_to_missing_snapshot,
                     "snapshot tree points to missing/incorrect snapshot:\n{}\n{}",
                     k.to_text(fs), found)? {
            return t.delete_at(iter, UpdateTriggerFlags::empty());
        }
    }

    let master = u32::from_le(st.master_subvol);
    if master == 0 {
        return Ok(t);
    }

    let repair = match subvolume::get(trans, master, false).found()? {
        None => fsck_err!(&t, id::snapshot_tree_to_missing_subvol,
                          "snapshot tree points to missing subvolume:\n{}", k.to_text(fs))?,
        Some(s) =>
            fsck_err_on!(&t, !snapshot::is_ancestor(trans, s.snapshot(), root_id),
                         id::snapshot_tree_to_wrong_subvol,
                         "snapshot tree points to subvolume that does not point to snapshot in this tree:\n{}",
                         k.to_text(fs))? ||
            fsck_err_on!(&t, s.snap(), id::snapshot_tree_to_snapshot_subvol,
                         "snapshot tree points to snapshot subvolume:\n{}", k.to_text(fs))?,
    };
    if !repair {
        return Ok(t);
    }

    let Some(subvol) = bch_err_fn!(fs, tree_master_subvol(&t, root_id))? else {
        return Ok(t);   /* nothing to be done here */
    };

    let mut u = t.bkey_make_mut(iter, k, UpdateTriggerFlags::empty(),
                                c::bch_bkey_type::KEY_TYPE_snapshot_tree,
                                size_of::<c::bkey_i_snapshot_tree>())?;
    u.k_i_mut().as_mut_snapshot_tree().expect("a snapshot tree key").master_subvol = subvol.to_le();
    Ok(t)
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

/// The node check_snapshot() is on: see the notes at the top.
struct Node<'i, 'k, 'a, 't> {
    iter: &'i mut BtreeIter<'t>,
    k:    BkeySC<'k>,
    /// The value as read, zero padded - until there's @u.
    v:    c::bch_snapshot,
    /// The key as it will be written, once anything has been repaired.
    u:    Option<TransBkey<'a, 't>>,
}

impl<'i, 'k, 'a, 't> Node<'i, 'k, 'a, 't> {
    fn new(iter: &'i mut BtreeIter<'t>, k: BkeySC<'k>) -> Option<Self> {
        let v = snapshot::val(k)?;
        Some(Node { iter, k, v, u: None })
    }

    fn id(&self) -> u32 {
        self.k.k.p.offset as u32
    }

    /// The node as it is now, repairs and all.
    fn v(&self) -> &c::bch_snapshot {
        match &self.u {
            Some(u) => BkeySC::from(u.k_i()).as_snapshot().expect("a snapshot key"),
            None    => &self.v,
        }
    }

    /// The node's key, made mutable and queued as its update if it isn't yet.
    fn u(&mut self, t: &TransAttempt<'a, 't>) -> Result<&mut TransBkey<'a, 't>, BchError> {
        if self.u.is_none() {
            self.u = Some(t.bkey_make_mut(self.iter, self.k, UpdateTriggerFlags::empty(),
                                          c::bch_bkey_type::KEY_TYPE_snapshot,
                                          size_of::<c::bkey_i_snapshot>())?);
        }
        Ok(self.u.as_mut().expect("just made"))
    }

    /// The node's value, to repair.
    fn v_mut(&mut self, t: &TransAttempt<'a, 't>) -> Result<&mut c::bch_snapshot, BchError> {
        Ok(snapshot_mut(self.u(t)?))
    }

    /// The key as it is now, to print.
    fn key(&self) -> BkeySC<'_> {
        match &self.u {
            Some(u) => BkeySC::from(u.k_i()),
            None    => BkeySC { k: self.k.k, v: self.k.v, iter: PhantomData },
        }
    }

    fn to_text<'f>(&self, fs: &'f Fs) -> BkeySCToText<'_, 'f> {
        self.key().to_text(fs)
    }
}

/// Is anything still pointing at node @s, @id - a subvolume whose snapshot
/// is us, our parent listing us as a child, or a child naming us as its
/// parent? For recovering a node whose state field is garbage: if it's
/// referenced it must be live, whatever the corrupted state says.
fn referenced(trans: &BtreeTrans<'_>, s: &c::bch_snapshot, id: u32) -> Result<bool, BchError> {
    if s.subvol() != 0 &&
       subvolume::get(trans, s.subvol(), false).found()?.is_some_and(|sv| sv.snapshot() == id) {
        return Ok(true);
    }

    if s.parent() != 0 &&
       lookup_node(trans, s.parent())?.is_some_and(|p| points_at(&p.v, Role::Parent, id)) {
        return Ok(true);
    }

    for child in s.children() {
        if child != 0 &&
           lookup_node(trans, child)?.is_some_and(|ch| points_at(&ch.v, Role::Child, id)) {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Recover the state field itself: stamp it from the legacy flags when
/// unset, decode a corrupted value back to the nearest codeword, and correct
/// a state left stale by a legacy flags-only tombstone.
fn check_state<'a, 't>(t: &TransAttempt<'a, 't>, n: &mut Node<'_, '_, 'a, 't>) -> Result<(), BchError> {
    let trans = t.trans();
    let fs = trans.fs();

    // A zero state field means the key predates the state field, or was
    // wiped: the legacy flag bits are then authoritative (old kernels
    // dual-wrote them). Derive the state from the bits whenever it's unset,
    // regardless of upgrade status - so a fs already on the new version but
    // carrying pre-state keys (never migrated, because no upgrade transition
    // ran) heals too. Mid-upgrade this is the expected migration and silent;
    // post-upgrade an unset state is unexpected, so surface it (autofix).
    if n.v().state_raw() == 0 {
        let upgrading = fs.version_upgrade_complete() <
            c::bcachefs_metadata_version::per_dev_fragmentation_lru;
        if upgrading ||
           fsck_err!(t, id::snapshot_state_bad,
                     "snapshot state unset, recovering from legacy flags:\n{}", n.to_text(fs))? {
            let state = n.v().state_from_flags();
            n.v_mut(t)?.state = (state as u32).to_le();
        }
    }

    // Pre-upgrade, the rewrite above always leaves a valid state, so this
    // only fires post-upgrade - where any invalid value (including zero) is
    // corruption. State-keyed repairs must not run on a state we can't read.
    if n.v().state_field().is_none() {
        let raw = n.v().state_raw();
        let (nearest, dist) = snapshot::state_nearest(raw);

        // Codewords are >= 14 apart, so anything <= 6 bits out is uniquely
        // decodable and recoverable. <= 2 bits is a genuine bitflip (its own
        // error, so failing hardware shows up in the counters); 3-6 is larger
        // but still-recoverable corruption. Further out is not a bitflip we
        // can trust.
        let fix = if dist <= 2 {
            fsck_err!(t, id::snapshot_state_bitflip,
                      "snapshot state {raw:#x} is a {dist}-bit flip of {nearest} - correcting:\n{}",
                      n.to_text(fs))?.then_some(nearest)
        } else if dist <= 6 {
            fsck_err!(t, id::snapshot_state_bad,
                      "snapshot state {raw:#x} is {dist} bits from {nearest} - correcting:\n{}",
                      n.to_text(fs))?.then_some(nearest)
        } else if referenced(trans, n.v(), n.id())? {
            // Too far from any codeword to decode. But the rest of the key is
            // intact, so if anything still references this node it must be
            // live.
            fsck_err!(t, id::snapshot_state_bad,
                      "snapshot state {raw:#x} is garbage, but the node is referenced - marking live:\n{}",
                      n.to_text(fs))?.then_some(SNAPSHOT_STATE_live)
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

    // A valid state that contradicts the legacy flags: current writers
    // dual-write both via set_state(), so only a legacy flags-only writer
    // leaves them disagreeing - the flags are the fresher write. But the flags
    // are single unprotected bits, so check the node's shape before trusting
    // them over a codeword: a node tombstoned by a legacy
    // bch2_snapshot_node_delete() carries the tombstone wipe, and no live,
    // will_delete or no_keys node ever has tree == 0. A bare DELETED bit on a
    // live-shaped node stays with the state field.
    //
    // Left as-is, the interior-deletion collector reads the stale no_keys
    // state, queues the tombstone, and the breadcrumb child pointer
    // fail-stops the edge checks - wedging every mount (the 2026-07-20 field
    // report; snapshot-inject/stale_state_tombstone).
    let v = n.v();
    if fsck_err_on!(t,
                    v.state_raw() != SNAPSHOT_STATE_deleted as u32 &&
                    v.deleted_obsolete() &&
                    v.tree == 0,
                    id::snapshot_state_stale_tombstone,
                    "snapshot spliced out by a legacy kernel (tombstone shape, legacy deleted flag)\n\
                     but the state field is stale at {} - correcting to deleted:\n{}",
                    StateName(v.state_field()), n.to_text(fs))? {
        n.v_mut(t)?.set_state(SNAPSHOT_STATE_deleted);
    }

    Ok(())
}

/// Data first, before anything else reasons about the state field: nothing
/// we write deletes a node with keys still accounted to it, so data means the
/// state field is the lie, whatever the rest of the node says. Settling it
/// here means every check after - and every edge repair a parent runs
/// against this node - sees a state that has already been held up against
/// the accounting.
///
/// No data: settled tombstone, and the checks after handle it.
///
/// How the state gets put right depends on which lie it is. deleted means
/// the node was spliced out of the tree, so reviving it means undoing that
/// splice. no_keys is only ever marked - it says "this node's keys have
/// migrated down", and the node stays where it is - so there is nothing to
/// relink and setting the state live is the whole repair.
fn check_has_data<'a, 't>(t: TransAttempt<'a, 't>, n: &mut Node<'_, '_, 'a, 't>) -> TransRet<'a, 't> {
    let fs = t.trans().fs();
    let state = n.v().state_field();

    if state != Some(SNAPSHOT_STATE_deleted) && state != Some(SNAPSHOT_STATE_no_keys) {
        return Ok(t);
    }
    let no_keys = state == Some(SNAPSHOT_STATE_no_keys);

    let mut breakdown = Printbuf::new();
    let (keys, sectors) = snapshot::accounting_totals(fs, n.id(), Some(&mut breakdown))?;

    if !fsck_err_on!(&t, keys != 0 || sectors != 0, id::snapshot_deleted_but_has_data,
                     "{} snapshot node has data accounted - {}:{}\n{}",
                     StateName(state),
                     if no_keys { "clearing state" } else { "undeleting" },
                     breakdown, n.to_text(fs))? {
        return Ok(t);
    }

    if no_keys {
        n.v_mut(&t)?.set_state(SNAPSHOT_STATE_live);
        Ok(t)
    } else {
        let u = n.u(&t)?;
        undelete_owns_data(t, u)
    }
}

/// A non-live state, checked against the child snapshots and the
/// subvolume. True if the node is a settled tombstone: no further checking.
fn check_deleted<'a, 't>(t: &TransAttempt<'a, 't>, n: &mut Node<'_, '_, 'a, 't>) -> Result<bool, BchError> {
    let trans = t.trans();
    let fs = trans.fs();
    let id = n.id();

    if n.v().state_raw() == SNAPSHOT_STATE_live as u32 {
        return Ok(false);
    }

    // A non-live node with two live children that both point back at it is a
    // lie: it's a branching interior that two subtrees still depend on as
    // their common ancestor, so it can't be gone. Reviving it is the simplest
    // repair back to a consistent tree.
    //
    // A single live child is legal, and does not trigger this: a no_keys node
    // is an emptied interior kept until the next remount, and it retains its
    // one live descendant so ancestry still resolves - non-live nodes are
    // single-child by construction.
    //
    // Children must reciprocate. A deleted node's child pointer can be a
    // splice breadcrumb: a child reparented to the grandparent that no longer
    // names us as parent doesn't depend on us and isn't counted.
    //
    // Before the deleted early-out, then fall through so the edge, depth and
    // tree checks validate the now-live node.
    let mut nr_live_children = 0;
    for child in n.v().children() {
        if child != 0 &&
           lookup_node(trans, child)?.is_some_and(|ch| ch.v.state() == Some(SNAPSHOT_STATE_live) &&
                                                      points_at(&ch.v, Role::Child, id)) {
            nr_live_children += 1;
        }
    }

    if fsck_err_on!(t, nr_live_children == 2, id::snapshot_deleted_has_live_children,
                    "snapshot marked {} but has two live children - reviving:\n{}",
                    StateName(n.v().state_field()), n.to_text(fs))? {
        n.v_mut(t)?.set_state(SNAPSHOT_STATE_live);
    }

    // Same check via the subvolume: deletion tombstones the subvolume in the
    // same transaction that marks its snapshot, so a non-live leaf whose
    // subvolume is live and points back has a bad state field. This also
    // fixes, in the same pass, a bad state stamped by the zero-state
    // migration in check_state() from a corrupt legacy flag. (A tombstoned
    // subvolume reads as ENOENT here: that's normal mid-deletion, not
    // evidence.)
    let v = n.v();
    if v.children()[0] == 0 && v.subvol() != 0 {
        let subvol_live = subvolume::get(trans, v.subvol(), false).found()?.is_some_and(|sv|
            sv.state() == Some(c::bch_subvolume_state::SUBVOLUME_STATE_live) && sv.snapshot() == id);

        if subvol_live &&
           fsck_err!(t, id::snapshot_deleted_but_subvol_live,
                     "snapshot marked {} but its subvolume is live - reviving:\n{}",
                     StateName(v.state_field()), n.to_text(fs))? {
            n.v_mut(t)?.set_state(SNAPSHOT_STATE_live);
        }
    }

    Ok(n.v().state_raw() == SNAPSHOT_STATE_deleted as u32)
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
fn points_at(s: &c::bch_snapshot, role: Role, other: u32) -> bool {
    match role {
        Role::Parent => s.children().contains(&other),
        Role::Child  => s.parent() == other,
    }
}

/// Point @s's @role pointer at @old to @new instead - 0 clears a child slot
/// - keeping the children in order.
fn set_ptr(s: &mut c::bch_snapshot, role: Role, old: u32, new: u32) {
    match role {
        Role::Child  => s.parent = new.to_le(),
        Role::Parent => {
            if let Some(child) = s.children.iter_mut().find(|c| u32::from_le(**c) == old) {
                *child = new.to_le();
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
fn parent_child_consistent(s: &c::bch_snapshot, id: u32, role: Role,
                           o: &c::bch_snapshot, o_id: u32) -> bool {
    let ((pa, pa_id), (ch, ch_id)) = match role {
        Role::Parent => ((s, id), (o, o_id)),
        Role::Child  => ((o, o_id), (s, id)),
    };

    ch_id < pa_id && pa.tree == ch.tree && pa.depth() + 1 == ch.depth()
}

/// Whether node @n, @n_id, as @role, has a pointer slot that's empty, or
/// whose current target doesn't reciprocate - and if so, the value that
/// would be displaced. Empty slots first, so a stale pointer keeps its own
/// shot at repair when its node is visited.
fn ptr_available(trans: &BtreeTrans<'_>, n: &c::bch_snapshot, n_id: u32, role: Role)
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
        if !lookup_node(trans, p)?.is_some_and(|t| points_at(&t.v, role.other(), n_id)) {
            return Ok(Some(p));
        }
    }

    Ok(None)
}

/// Does snapshot @id own anything at all?
///
/// Any nonzero counter in any snapshotted btree says yes - keys, their
/// bytes, or sectors. Only the existence of evidence matters here, not what
/// it means, so there's nothing to version gate: a filesystem from before the
/// per-btree key counts still accounts sectors, and on one that has them a
/// snapshot holding only dirents or xattrs shows up in nr_keys where sectors
/// alone would read as empty.
///
/// In-memory read: current as of the last applied delta, so unlike a btree
/// read it needs no write buffer flush to be trustworthy.
fn has_accounting(fs: &Fs, id: u32) -> bool {
    crate::BTREE_IDS_KNOWN.iter()
        .filter(|&&btree| crate::BTREE_HAS_SNAPSHOTS_MASK & (1 << btree as u32) != 0)
        .any(|&btree| {
            let mut v = [0u64; 3];
            accounting::mem_read(fs, DiskAccountingKind::Snapshot { id, btree: btree as u32 }.encode(),
                                 &mut v);
            v.iter().any(|&x| x != 0)
        })
}

/// Commit an edge repair, have the table rebuilt, and restart: decisions
/// only see settled state. The commit first - the restart would discard the
/// repair.
fn edge_repair_commit<'a, 't>(t: TransAttempt<'a, 't>) -> TransRet<'a, 't> {
    let fs = t.trans().fs();
    let t = t.commit(None, CommitFlags::NO_ENOSPC)?;
    snapshot::set_need_table_rebuild(fs);
    Err(t.restart(bch_errcode::BCH_ERR_transaction_restart_nested))
}

/// Put a node the accounting says is alive, @u, back into the tree.
///
/// Two children means nothing ever spliced this node out: the structure is
/// intact and only the state field is wrong, so setting it live is the whole
/// repair - snapshot::node_undelete() is for undoing a splice, and rejects
/// that shape outright.
///
/// Otherwise it relinks through the parent, which has to be live first.
fn undelete_owns_data<'a, 't>(t: TransAttempt<'a, 't>, u: &mut TransBkey<'_, 't>) -> TransRet<'a, 't> {
    let s = snapshot_mut(u);
    if s.children()[1] != 0 {
        s.set_state(SNAPSHOT_STATE_live);
        return Ok(t);
    }

    let t = undelete_ancestors(t, s.parent())?;
    snapshot::node_undelete(t, u)
}

/// The highest dead node from @id up: the one whose own parent is still in
/// the tree.
fn topmost_dead_ancestor(trans: &BtreeTrans<'_>, mut id: u32) -> Result<Option<u32>, BchError> {
    let mut topmost = None;

    while id != 0 {
        // a missing parent is undelete's to report, against the node naming it
        let Some(s) = lookup_node(trans, id)? else { break };
        if s.v.state() != Some(SNAPSHOT_STATE_deleted) {
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

/// Undelete relinks through the parent, so refusing a fully condemned chain
/// turns a repairable filesystem into emergency read-only.
///
/// One node per commit: the chain is unbounded.
///
/// Nothing corroborates these ancestors - they are scaffolding, and stay
/// only because the node below them ends up live. depth and the skiplists
/// are left stale for snapshot_bad_depth/snapshot_bad_skiplist.
fn undelete_ancestors<'a, 't>(t: TransAttempt<'a, 't>, id: u32) -> TransRet<'a, 't> {
    let Some(topmost) = topmost_dead_ancestor(t.trans(), id)? else { return Ok(t) };

    let mut u = get_mut_node(&t, topmost)?;
    let t = undelete_owns_data(t, &mut u)?;
    edge_repair_commit(t)
}

/// Put back a child node, @id under @parent_id, that isn't there at all.
///
/// Same argument as undeleting a tombstone that still owns data, one step
/// further along: nothing we write removes a snapshot node with keys still
/// accounted to it, so a node that's missing while its keys are not is a
/// node that was destroyed, not deleted. Its keys are still on disk and
/// still reachable from every view that inherits from it - only the node
/// naming that view is gone. Recreate it, and they stay reachable.
///
/// A leaf, because that's the shape we can justify: @parent_id names it as a
/// child, so parent, tree, depth and skiplists all follow from a node we can
/// see, and nothing attests to any children of its own. If it did have
/// descendants, their own parent pointers bring them back on a later pass
/// through the same edge check.
///
/// The subvolume, if one claims this id, comes back with it: a subvolume
/// whose snapshot went missing is exactly what a resurrected leaf should
/// carry, and zeroing it would strand the subvolume instead.
fn resurrect_child<'a, 't>(t: TransAttempt<'a, 't>, parent_id: u32, id: u32) -> TransRet<'a, 't> {
    let trans = t.trans();
    let fs = trans.fs();

    let Some(parent) = lookup_node(trans, parent_id)? else {
        return Err(fs.err(bch_errcode::BCH_ERR_EINVAL_snapshot_edge_to_missing_node).into());
    };

    let mut n = t.bkey_alloc_typed::<c::bkey_i_snapshot>()?;
    n.k_mut().p = pos(0, id as u64);

    let v = snapshot_mut(&mut n);
    v.parent    = parent_id.to_le();
    v.tree      = parent.v.tree;
    v.depth     = snapshot::depth(fs, parent_id).to_le();
    v.btime.lo  = fs.current_time().to_le();
    for skip in &mut v.skip {
        *skip = snapshot::skiplist_get(fs, parent_id).to_le();
    }
    v.skip.sort_unstable_by_key(|s| u32::from_le(*s));
    v.subvol    = subvol_claiming(trans, id)?.unwrap_or(0).to_le();
    v.set_state(SNAPSHOT_STATE_live);

    snapshot::table_make_room(fs, id)?;
    t.insert(c::btree_id::snapshots, n, UpdateTriggerFlags::empty())
}

/// The nodes an edge repair is between, keys and all. Which pointer is wrong
/// is only decidable from parent/children/tree/depth across both sides, so a
/// message naming them by id reports the conclusion and none of the evidence
/// - and a field report is whatever the message printed.
struct EdgeNodes<'n, 'f> {
    fs:       &'f Fs,
    node:     BkeySC<'n>,
    target:   Option<&'n c::bkey_i_snapshot>,
    claimant: Option<&'n c::bkey_i_snapshot>,
}

impl<'n, 'f> EdgeNodes<'n, 'f> {
    fn new(
        fs:       &'f Fs,
        node:     &'n Node<'_, '_, '_, '_>,
        target:   Option<&'n c::bkey_i_snapshot>,
        claimant: Option<&'n c::bkey_i_snapshot>,
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
fn past_deleted(trans: &BtreeTrans<'_>, s: &c::bch_snapshot, role: Role)
    -> Result<Option<u32>, BchError>
{
    let next = |s: &c::bch_snapshot| match role {
        Role::Child  => s.parent(),
        Role::Parent => s.children()[0],
    };

    let mut id = next(s);
    for _ in 0..c::BTREE_MAX_DEPTH as u32 * 64 {
        if id == 0 {
            return Ok(Some(0));
        }
        match lookup_node(trans, id)? {
            None => return Ok(Some(0)),
            Some(s) if s.v.state() != Some(SNAPSHOT_STATE_deleted) => return Ok(Some(id)),
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
    n:        &Node<'_, '_, '_, '_>,
    role:     Role,
    other_id: u32,
) -> Result<Option<EdgeRepair>, BchError> {
    let trans = t.trans();
    let fs = trans.fs();
    let id = n.id();
    let ptr = role.ptr_name();

    let other = lookup_node(trans, other_id)?;

    // Connectivity: the live tree must be closed over not-deleted nodes. A
    // deleted target is history - an interrupted deletion left our edge
    // pointing into it - so rewrite our own edge past it: parent edges walk up
    // the tombstone's retained parent chain, child edges walk down retained
    // children, to the nearest not-deleted node (or nothing, if that
    // direction is all dead). The tombstone itself is untouched; depth and
    // skiplist fallout is repaired by the checks downstream.
    //
    // Only for a tombstone that owns no data - see below. We cannot lean on
    // check_deleted() having already adjudicated the target: the walk is
    // reverse from POS_MAX and snapshot ids descend from the root, so a parent
    // always reaches a deleted child's edge before that child is visited at
    // all.
    if let Some(o) = other.as_ref().filter(|o| o.v.state() == Some(SNAPSHOT_STATE_deleted)) {
        // Data is definitive: nothing we write deletes a node with keys still
        // accounted to it, so a deleted node that still owns data has a state
        // field that lies, and routing the tree around it strands those keys.
        // Undelete it and leave our edge alone - the node's own visit
        // validates the result.
        let (keys, sectors) = snapshot::accounting_totals(fs, other_id, None)?;
        if keys != 0 || sectors != 0 {
            let fix = fsck_err!(t, id::snapshot_deleted_but_has_data,
                                "snapshot {id} {ptr} pointer {other_id} is deleted but has {keys} keys, \
                                 {sectors} sectors accounted - undeleting rather than splicing past it:{}",
                                EdgeNodes::new(fs, n, Some(o), None))?;
            return Ok(fix.then_some(EdgeRepair::Undelete(other_id)));
        }

        let Some(repl) = past_deleted(trans, &o.v, role)? else { return Ok(None) };
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
            if let Some(old) = ptr_available(trans, &o.v, other_id, role.other())? {
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
    if let Some(repl) = snapshot::table_find_edge(fs, n.v(), id, role == Role::Child) {
        if let Some(r) = lookup_node(trans, repl)? {
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

    // A child pointer naming a node that isn't there. The accounting says
    // which way to go: keys accounted to it mean the node was destroyed rather
    // than deleted, and putting it back is what keeps them reachable. Nothing
    // accounted means there's nothing to lose, and clearing is the smaller
    // repair - which also means a garbage child id gets cleared instead of
    // conjuring a snapshot out of one stomped field.
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

    // A single stomped field always leaves the other side's intact pointer for
    // the repairs above to key off, so reaching here means multiple
    // corruptions or a destroyed node - beyond what local evidence can
    // repair. Rare enough that we report and stop rather than attempt
    // topology surgery on a conjunction of corruptions:
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
    t:        TransAttempt<'a, 't>,
    n:        &mut Node<'_, '_, 'a, 't>,
    role:     Role,
    other_id: u32,
) -> TransRet<'a, 't> {
    let Some(repair) = edge_repair(&t, n, role, other_id)? else { return Ok(t) };

    let t = match repair {
        EdgeRepair::Undelete(id) => {
            let mut u = get_mut_node(&t, id)?;
            undelete_owns_data(t, &mut u)?
        }
        EdgeRepair::Retarget(new) => {
            set_ptr(n.v_mut(&t)?, role, other_id, new);
            t
        }
        EdgeRepair::Complete(old) => {
            set_ptr(snapshot_mut(&mut get_mut_node(&t, other_id)?), role.other(), old, n.id());
            t
        }
        EdgeRepair::Resurrect => resurrect_child(t, n.id(), other_id)?,
    };
    edge_repair_commit(t)
}

/// Whether @id's tree pointer, @tree, names a tree that exists and whose
/// root @id descends from.
fn tree_ptr_good(trans: &BtreeTrans<'_>, id: u32, tree: u32) -> Result<bool, BchError> {
    let Some(st) = snapshot::tree_lookup(trans, tree).found()? else { return Ok(false) };
    Ok(snapshot::is_ancestor_early(trans.fs(), id, u32::from_le(st.root_snapshot)))
}

/// Node @n's tree pointer was wrong: make sure its root's is right -
/// creating a tree if there isn't one for it - and point @n at that.
fn tree_ptr_repair<'a, 't>(t: &TransAttempt<'a, 't>, n: &mut Node<'_, '_, 'a, 't>) -> Result<(), BchError> {
    let trans = t.trans();
    let fs = trans.fs();
    let id = n.id();
    let root_id = snapshot::root(fs, id);

    let mut tree_id = if root_id == id {
        n.v().tree()
    } else {
        snapshot::lookup_key(trans, root_id)?.v.tree()
    };

    let tree = snapshot::tree_lookup(trans, tree_id).found()?;
    if tree.is_none_or(|st| u32::from_le(st.root_snapshot) != root_id) {
        let mut new = snapshot::tree_create(t)?;
        let v = new.k_i_mut().as_mut_snapshot_tree().expect("a snapshot tree key");
        v.master_subvol = snapshot::oldest_subvol(fs, root_id).unwrap_or(0).to_le();
        v.root_snapshot = root_id.to_le();
        tree_id = new.k().p.offset as u32;

        if root_id != id {
            snapshot_mut(&mut get_mut_node(t, root_id)?).tree = tree_id.to_le();
        }
    }

    if n.v().tree() != tree_id {
        n.v_mut(t)?.tree = tree_id.to_le();
    }
    Ok(())
}

/// Depth is derived from the parent - and the parent was visited first.
fn check_depth<'a, 't>(t: &TransAttempt<'a, 't>, n: &mut Node<'_, '_, 'a, 't>) -> Result<(), BchError> {
    let trans = t.trans();
    let fs = trans.fs();
    let parent_id = n.v().parent();

    // With the parent missing there's nothing to derive depth from - and "no
    // parent" would give 0, flattening an interior node to a root.
    // check_edge() has already reported the dangling pointer; leave depth
    // alone until that's resolved and the node is visited again.
    let real_depth = if parent_id == 0 {
        0
    } else {
        let Some(parent) = lookup_node(trans, parent_id)? else { return Ok(()) };
        parent.v.depth() + 1
    };

    if fsck_err_on!(t, n.v().depth() != real_depth, id::snapshot_bad_depth,
                    "snapshot with incorrect depth field, should be {real_depth}:\n{}",
                    n.to_text(fs))? {
        n.v_mut(t)?.depth = real_depth.to_le();
    }
    Ok(())
}

/// Every skiplist entry is an ancestor - none, for a root.
fn check_skiplists<'a, 't>(t: &TransAttempt<'a, 't>, n: &mut Node<'_, '_, 'a, 't>) -> Result<(), BchError> {
    let trans = t.trans();
    let fs = trans.fs();
    let id = n.id();

    for i in 0..n.v().skip.len() {
        let parent_id = n.v().parent();
        let skip = u32::from_le(n.v().skip[i]);

        let bad = if parent_id == 0 { skip != 0 } else { !snapshot::is_ancestor_early(fs, id, skip) };
        if !bad {
            continue;
        }

        let mut msg = Printbuf::new();
        write!(msg, "snapshot with bad skiplist pointer {skip}:\n{}\n", n.to_text(fs));
        if skip != 0 {
            let mut iter = BtreeIter::new(trans, c::btree_id::snapshots, pos(0, skip as u64),
                                          BtreeIterFlags::empty());
            let skip_k = iter.peek_slot()?.expect("a slot always has a key");
            write!(msg, "points to\n  {}\n", skip_k.to_text(fs));
        }

        if fsck_err!(t, id::snapshot_bad_skiplist, "{msg}")? {
            n.v_mut(t)?.skip[i] = snapshot::skiplist_get(fs, parent_id).to_le();
        }
    }

    // Kept sorted: is_ancestor() tries the highest first.
    if n.u.is_some() {
        n.v_mut(t)?.skip.sort_unstable_by_key(|s| u32::from_le(*s));
    }
    Ok(())
}

/// The subvolume backref, against the subvolume it names.
fn check_to_subvol<'a, 't>(t: TransAttempt<'a, 't>, n: &mut Node<'_, '_, 'a, 't>) -> TransRet<'a, 't> {
    let trans = t.trans();
    let fs = trans.fs();
    let id = n.id();

    let state = n.v().state_field();
    let snap_deleting = state == Some(SNAPSHOT_STATE_will_delete);
    let should_have_subvol = n.v().children()[0] == 0 &&
        (state == Some(SNAPSHOT_STATE_live) || snap_deleting);

    if n.v().subvol() != 0 {
        let subvol_id = n.v().subvol();

        // A raw read: subvolume::get() reports deleted subvolumes as ENOENT,
        // and the message should show what's actually there.
        let mut iter = BtreeIter::new(trans, c::btree_id::subvolumes, pos(0, subvol_id as u64),
                                      BtreeIterFlags::empty());
        let subvol_k = iter.peek_slot()?.expect("a slot always has a key");
        let subvol = subvolume::val(subvol_k);

        // A missing subvolume can be rebuilt from right here, and only from
        // here: this snapshot names it, and a snapshot carrying a subvol
        // backref is a leaf - which is what reconstruct_subvol() needs and
        // what its other callers can't promise, since an inode's or dirent's
        // snapshot may be interior. Left to them, a subvolume whose key was
        // lost after it had been snapshotted was never reconstructed at all.
        //
        // Not while the snapshot is deleting, though: there the missing
        // subvolume is a tombstoned deletion in flight, and rebuilding it
        // would revert it.
        if subvol.is_none() && !snap_deleting {
            match check::reconstruct_subvol_root(trans, id, subvol_id, None) {
                Ok(root) => return check::reconstruct_subvol(t, id, subvol_id, root),
                // couldn't find a root inode for it - fall through and report
                Err(e) if e.matches(bch_errcode::BCH_ERR_fsck_repair_unimplemented) => {}
                Err(e) => return Err(e.into()),
            }
        }

        // Wrong backref, or a missing subvolume we couldn't rebuild: repair
        // needs the subvolume side validated first - it belongs to the
        // dedicated pass after check_subvols. Report only; an error return
        // here would regress mounts of filesystems mid-deletion.
        let subvol = match subvol {
            Some(s) if s.snapshot() == id => s,
            Some(_) => {
                fsck_err_report!(fs, id::snapshot_subvol_backref_wrong,
                                 "snapshot's subvolume doesn't point back at it:\n{}\n{}",
                                 n.to_text(fs), subvol_k.to_text(fs));
                return Ok(t);
            }
            None => {
                fsck_err_report!(fs, id::snapshot_subvol_backref_wrong,
                                 "snapshot points to missing subvolume {subvol_id}:\n{}",
                                 n.to_text(fs));
                return Ok(t);
            }
        };

        // The deletion machinery couples exactly one bit on each side: a
        // snapshot is will_delete iff its subvolume is tombstoned (live vs
        // unlinked is the subvolume's own user-visibility business, invisible
        // to the snapshot). With the edge intact, a state mismatch repairs in
        // one direction only: the subvolume implies the snapshot state
        // exactly, while the reverse would have to guess between live and
        // unlinked.
        let subvol_deleted = subvol.state() == Some(c::bch_subvolume_state::SUBVOLUME_STATE_deleted);
        if fsck_err_on!(&t, snap_deleting != subvol_deleted, id::snapshot_subvol_state_mismatch,
                        "snapshot {} but its subvolume is {}:\n{}\n{}",
                        if snap_deleting { "will_delete" } else { "live" },
                        if subvol_deleted { "deleted" } else { "not deleted" },
                        n.to_text(fs), subvol_k.to_text(fs))? {
            n.v_mut(&t)?.set_state(if subvol_deleted {
                SNAPSHOT_STATE_will_delete
            } else {
                SNAPSHOT_STATE_live
            });
        }
    } else if should_have_subvol && state == Some(SNAPSHOT_STATE_live) {
        // A live leaf with no backref: a subvolume still pointing at it means
        // the backref was wiped - restore it. (A second claimant, if damage
        // minted one, still hits check_subvols' doesn't-point-back
        // fail-stop.) No claimant is an orphan leaf, whose repair (creating a
        // subvolume) is unimplemented, as above.
        if let Some(subvol) = subvol_claiming(trans, id)? {
            if fsck_err!(&t, id::snapshot_subvol_backref_wrong,
                         "snapshot leaf missing subvol backref, subvolume {subvol} points at it - restoring:\n{}",
                         n.to_text(fs))? {
                let v = n.v_mut(&t)?;
                v.subvol = subvol.to_le();
                v.set_subvol_obsolete(true);
            }
        }
    }

    if fsck_err_on!(&t, n.v().subvol() != 0 && !should_have_subvol, id::snapshot_should_not_have_subvol,
                    "snapshot should not point to subvol:\n{}", n.to_text(fs))? {
        if n.v().children()[0] != 0 {
            return Err(fs.err(bch_errcode::BCH_ERR_fsck_repair_unimplemented).into());
        }

        // XXX: DANGEROUS
        n.v_mut(&t)?.subvol = 0;
    }

    // Live nodes only: the _OBSOLETE flags are old-format compat bits, and
    // set_state() clears SUBVOL_OBSOLETE for every non-live state even when
    // the subvol backref is retained (a will_delete leaf keeps it; deletion
    // checks it) - old kernels must not read a dying snapshot as a live
    // subvolume leaf:
    let v = n.v();
    if v.state_raw() == SNAPSHOT_STATE_live as u32 && v.subvol_obsolete() != (v.subvol() != 0) {
        let mut msg = Printbuf::new();
        write!(msg, "snapshot node {id} has wrong subvol flag:\n{}", n.to_text(fs));

        if v.subvol() != 0 {
            let mut iter = BtreeIter::new(trans, c::btree_id::subvolumes, pos(0, v.subvol() as u64),
                                          BtreeIterFlags::empty());
            let subvol_k = iter.peek_slot()?.expect("a slot always has a key");
            write!(msg, "\n{}", subvol_k.to_text(fs));
        }

        if fsck_err!(&t, id::snapshot_subvol_flag_wrong, "{msg}")? {
            let has_subvol = n.v().subvol() != 0;
            n.v_mut(&t)?.set_subvol_obsolete(has_subvol);
        }
    }

    Ok(t)
}

fn check_snapshot<'a, 't>(
    t:    TransAttempt<'a, 't>,
    iter: &mut BtreeIter<'t>,
    k:    BkeySC<'_>,
) -> TransRet<'a, 't> {
    let trans = t.trans();
    let fs = trans.fs();

    let Some(mut n) = Node::new(iter, k) else { return Ok(t) };

    check_state(&t, &mut n)?;
    let t = check_has_data(t, &mut n)?;

    if check_deleted(&t, &mut n)? {
        return Ok(t);
    }

    let mut t = t;
    let parent = n.v().parent();
    if parent != 0 {
        t = check_edge(t, &mut n, Role::Child, parent)?;
    }
    for child in n.v().children() {
        if child != 0 {
            t = check_edge(t, &mut n, Role::Parent, child)?;
        }
    }

    if !tree_ptr_good(trans, n.id(), n.v().tree())? &&
       fsck_err!(&t, id::snapshot_to_bad_snapshot_tree,
                 "snapshot points to missing/incorrect tree:\n{}", n.to_text(fs))? {
        tree_ptr_repair(&t, &mut n)?;
    }

    check_depth(&t, &mut n)?;
    check_skiplists(&t, &mut n)?;
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
#[no_mangle]
pub unsafe extern "C" fn bch2_check_snapshots_trans(trans: *mut c::btree_trans) -> core::ffi::c_int {
    let fs = unsafe { Fs::borrow_raw((*trans).c) };
    let trans = unsafe { BtreeTrans::borrow_raw(&fs, trans) };

    match check_snapshots_trans(&trans) {
        Ok(())  => 0,
        Err(e)  => -e.raw(),
    }
}

fn check_snapshots(fs: &Fs) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    check_snapshots_trans(&trans)?;

    // The pass completed cleanly, so the snapshots btree is consistent: record
    // it so check_key_has_snapshot can trust the in-memory snapshot table (see
    // bch2_btree_is_clean()). The same gate the pass runner uses to mark a
    // pass complete.
    if !fs.flag(c::bch_fs_flags::BCH_FS_error) {
        passes::set_btree_clean(fs, c::btree_id::snapshots);
    }
    Ok(())
}

crate::recovery_pass!(bch2_check_snapshots => check_snapshots);

/* reconstruct_snapshots: */

/// A list of snapshot ids, space separated: as bch2_snapshot_id_list_to_text().
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
fn recreate_node<'a, 't>(t: TransAttempt<'a, 't>, id: u32) -> TransRet<'a, 't> {
    let trans = t.trans();
    let fs = trans.fs();

    let tree_id = find_key(trans, c::btree_id::snapshot_trees, |k| {
        k.as_snapshot_tree().is_some_and(|st| u32::from_le(st.root_snapshot) == id)
    })?;

    let tree_id = match tree_id {
        Some(tree_id) => tree_id,
        None => {
            let mut tree = snapshot::tree_create(&t)?;
            tree.k_i_mut().as_mut_snapshot_tree().expect("a snapshot tree key").root_snapshot =
                id.to_le();
            tree.k().p.offset as u32
        }
    };

    let mut n = t.bkey_alloc_typed::<c::bkey_i_snapshot>()?;
    n.k_mut().p = pos(0, id as u64);

    let v = snapshot_mut(&mut n);
    v.tree     = tree_id.to_le();
    v.btime.lo = fs.current_time().to_le();
    v.subvol   = subvol_claiming(trans, id)?.unwrap_or(0).to_le();
    v.set_state(SNAPSHOT_STATE_live);

    snapshot::table_make_room(fs, id)?;
    t.insert(c::btree_id::snapshots, n, UpdateTriggerFlags::empty())
}

/// Recreate node @id of @tree if it's missing - only if it's a tree to
/// itself: one we can't place.
fn reconstruct_node<'a, 't>(t: TransAttempt<'a, 't>, id: u32, tree: &[u32]) -> TransRet<'a, 't> {
    let fs = t.trans().fs();

    if fsck_err_on!(&t, snapshot::id_state(fs, id) == IdState::SNAPSHOT_ID_empty,
                    id::snapshot_node_missing,
                    "snapshot node {id} from tree {} missing, recreate?", IdList(tree))? {
        if tree.len() > 1 {
            bch_err!(fs, "cannot reconstruct snapshot trees with multiple nodes");
            return Err(fs.err(bch_errcode::BCH_ERR_fsck_repair_unimplemented).into());
        }

        return recreate_node(t, id);
    }

    Ok(t)
}

fn reconstruct_snapshots(fs: &Fs) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);

    let mut btrees: KVVec<c::btree_id> = KVVec::new();
    for &btree in crate::BTREE_IDS_KNOWN {
        if crate::BTREE_HAS_SNAPSHOTS_MASK & (1 << btree as u32) != 0 {
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
            let t = progress.update(t, iter)?;
            r.add(k.k.p)?;
            Ok(t)
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

/// A key moved out of a deleted snapshot to its live descendant @snapshot -
/// the snapshot-deletion scan's premise, "a key in snapshot X implies an
/// inode in snapshot X", has to hold at the destination too, or the key gets
/// stranded again on the next deletion. If the inode is only inherited from
/// an ancestor of the descendant, copy it down. A genuinely missing inode is
/// left for a full fsck to reconstruct; we don't do that or schedule passes
/// here.
fn key_has_inode_in_snapshot<'a, 't>(
    t:        TransAttempt<'a, 't>,
    btree:    c::btree_id,
    inum:     u64,
    snapshot: u32,
) -> TransRet<'a, 't> {
    match btree {
        c::btree_id::extents | c::btree_id::dirents | c::btree_id::xattrs => {}
        _ => return Ok(t),
    }

    let Some(mut inode) = inode::find_by_inum_snapshot(t.trans(), inum, snapshot,
                                                       BtreeIterFlags::empty()).found()? else {
        return Ok(t);
    };
    if inode.bi_snapshot == snapshot {
        return Ok(t);
    }

    inode.bi_snapshot = snapshot;
    inode::fsck_write(t, &mut inode)
}

/// Key @k is in a snapshot that's missing or dead: repair it - deleted, or
/// moved to a live descendant - true if so, for the caller to skip it. With
/// no @iter - the promote path - it can't be repaired, and is just skipped.
fn check_key_has_snapshot<'a, 't>(
    t:    TransAttempt<'a, 't>,
    iter: Option<&mut c::btree_iter>,
    k:    BkeySC<'_>,
) -> TransResult<'a, 't, bool> {
    let trans = t.trans();
    let fs = trans.fs();
    let state = snapshot::id_state(fs, k.k.p.snapshot);

    let Some(iter) = iter else { return t.done(true) };

    if state == IdState::SNAPSHOT_ID_live {
        return t.done(false);
    }

    let mut buf = Printbuf::new();
    let mut ret = Ok(());
    let mut keep = |r: Result<(), BchError>| if r.is_err() { ret = r };

    // The incomplete snapshot deletion that stranded this key almost
    // certainly stranded sibling keys across the content btrees too - left
    // alone they only surface later, when copygc/reconcile trips over them.
    // Schedule the content passes so the whole cascade of damage gets
    // repaired in this fsck run rather than festering. run_explicit() only
    // returns the unwind error (deferring this key's own repair) when it
    // actually needs to rewind to an earlier pass; scheduling a
    // later-or-current pass just marks it to run.
    //
    // skip_if_complete: at most one sweep per instance. These passes don't
    // repair every stranded-key state - if they already ran and this key is
    // still here, rescheduling can't fix it, but it would re-arm the passes in
    // the superblock on every encounter, forcing fsck on every mount.
    for pass in [BCH_RECOVERY_PASS_check_inodes, BCH_RECOVERY_PASS_check_extents,
                 BCH_RECOVERY_PASS_check_dirents, BCH_RECOVERY_PASS_check_xattrs] {
        keep(passes::run_explicit(fs, &mut buf, pass,
                                  c::bch_run_recovery_pass_flags::RUN_RECOVERY_PASS_skip_if_complete));
    }

    // A key in a deleted snapshot means that snapshot has data, and
    // check_snapshot() holds a deleted node up against the accounting and
    // undeletes it when it does - nothing we write deletes a node with keys
    // still accounted to it, so the state field is the lie. If we are standing
    // here looking at the key, that corroboration hasn't happened: either
    // check_snapshots hasn't run, or it ran before whatever put the node in
    // this state.
    //
    // So require it, and let the node be undeleted before anything below
    // decides what to do with the key - the alternatives there are deleting it
    // or migrating it to a live descendant, and both are irreversible on the
    // strength of a state field the accounting disagrees with.
    //
    // Requiring it rewinds if we are past it, so the key is retried once the
    // node is live again. If check_snapshots has already run this recovery and
    // the key is still in a deleted snapshot, the corroboration saw it and let
    // it stand - require() returns Ok and the repair below proceeds.
    if state == IdState::SNAPSHOT_ID_deleted {
        keep(passes::require(fs, &mut buf, BCH_RECOVERY_PASS_check_snapshots));
    }

    // Both repairs below destroy or relocate a key based on the in-memory
    // snapshot table. Only trust it if the snapshots and subvolumes btrees
    // have both been validated consistent (by check_snapshots /
    // check_subvols) and not mutated since. If they haven't, the table may
    // simply be stale and acting on it would destroy live data; schedule the
    // passes and defer instead. Unlike require() on its own, this doesn't
    // trust a pass that merely ran this mount (or was ratelimited) - it must
    // have run since the last mutation.
    if !passes::btree_is_clean(fs, c::btree_id::snapshots) ||
       !passes::btree_is_clean(fs, c::btree_id::subvolumes) {
        keep(passes::require(fs, &mut buf, BCH_RECOVERY_PASS_check_snapshots));
        keep(passes::require(fs, &mut buf, BCH_RECOVERY_PASS_check_subvols));
    }

    ret?;

    write!(buf, "{} {}", iter.btree_id(), k.to_text(fs));

    if state == IdState::SNAPSHOT_ID_deleted {
        // Reached at runtime - data moves (bch2_data_update_init()) - not in
        // fsck: check_allocations rebuilds the accounting from the keys first,
        // so a deleted node with keys has data accounted, and check_snapshots
        // undeletes it (snapshot_deleted_but_has_data) before any pass gets
        // here. (fsck-inject test_deleted_interior_has_data.)
        //
        // If there's no live descendant (a leaf, or an interior node whose
        // subtree is entirely deleted) the key is genuinely orphaned - nothing
        // can see it - so delete it. If there is a live descendant the key is
        // still visible to it via inheritance and should have been migrated
        // there during deletion; migrate it now rather than dropping it (the
        // deleted node retains a child pointer so live_descendent() can find
        // the target).
        //
        // An error finding it - a dangling child pointer, on a table
        // check_snapshots just validated clean (we only reach here clean) - is
        // a real inconsistency, not a stale table. Surface it; don't destroy
        // the key.
        match snapshot::live_descendent(fs, k.k.p.snapshot)? {
            None => if inode_fsck_err!(trans, k.k.p, id::bkey_in_deleted_snapshot,
                                       "key in deleted snapshot {buf}, delete?")? {
                let t = t.delete_at_raw(iter, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
                return t.done(true);
            },
            Some(live_child) => if fsck_err!(trans, id::bkey_in_deleted_interior_snapshot,
                                             "key in deleted interior snapshot {buf}, migrating to live descendant {live_child}")? {
                let (btree, inum) = (iter.btree_id(), k.k.p.inode);
                let t = snapshot::delete_dead_key(t, iter, k, live_child)?;
                let t = key_has_inode_in_snapshot(t, btree, inum, live_child)?;
                return t.done(true);
            },
        }
    } else if inode_fsck_err!(trans, k.k.p, id::bkey_in_missing_snapshot,
                              "key in missing snapshot {buf}, delete?")? {
        let t = t.delete_at_raw(iter, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
        return t.done(true);
    }

    t.done(false)
}

/// check_key_has_snapshot(), for C's bch2_check_key_has_snapshot(): 1 if @k
/// was repaired or can't be, for the caller to skip it.
///
/// # Safety
/// The arguments are C's: @trans live with an attempt in progress, @iter NULL
/// - the promote path - or the iterator @k was read from.
#[no_mangle]
pub unsafe extern "C" fn __bch2_check_key_has_snapshot(
    trans: *mut c::btree_trans,
    iter:  Option<&mut c::btree_iter>,
    k:     c::bkey_s_c,
) -> core::ffi::c_int {
    let fs = unsafe { Fs::borrow_raw((*trans).c) };
    let trans = unsafe { BtreeTrans::borrow_raw(&fs, trans) };
    let t = trans.attempt_in_progress();

    match check_key_has_snapshot(t, iter, BkeySC::from(&k)) {
        Ok((_, handled)) => handled as core::ffi::c_int,
        Err(e)           => -BchError::from(e).raw(),
    }
}
