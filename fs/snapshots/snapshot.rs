// SPDX-License-Identifier: GPL-2.0

//! Snapshots: queries of the in-memory table, and lookups in the btrees.

/* DOC_LATEX(snapshots)
 * \subsubsection{Overview}
 *
 * A subvolume is an independent directory tree within the filesystem, similar to
 * btrfs subvolumes. Subvolumes appear as directories and can be created empty or
 * as snapshots of existing subvolumes. Snapshots are writeable by default and can
 * be snapshotted again, forming a tree of snapshots. They can also be created
 * read-only.
 *
 * Snapshots are O(1) to create regardless of filesystem size --- no data or
 * metadata is copied. Writes to either the source or the snapshot only diverge
 * where modifications occur. Many thousands or millions of snapshots can exist,
 * limited only by disk space.
 *
 * Each snapshot tree has a \emph{master subvolume}: the original non-snapshot
 * subvolume from which all snapshots in the tree descend. The master subvolume is
 * significant for quota accounting: quotas are charged based on the uid/gid/project
 * recorded in the master subvolume's inodes. Snapshot subvolumes bypass quota
 * enforcement entirely, because ownership changes within a snapshot would make
 * it ambiguous which quota should be charged. If the master subvolume is deleted,
 * quota accounting for that snapshot tree is skipped.
 *
 * Subvolumes and snapshots can be managed with:
 * \begin{itemize}
 * \item \texttt{bcachefs subvolume create/delete/snapshot} --- create, delete,
 *   and snapshot subvolumes
 * \item \texttt{bcachefs subvolume list} --- list subvolumes in tree view
 * \item \texttt{bcachefs subvolume list-snapshots} --- show snapshot tree with
 *   per-snapshot disk usage
 * \end{itemize}
 *
 * \subsubsection{Architecture}
 *
 * A subvolume holds a root inode number and a snapshot ID. The snapshot ID links
 * the subvolume to a leaf node in the snapshots btree, which records the snapshot
 * tree structure: parent, children (up to two), depth, and a skiplist for fast
 * ancestor queries. Only leaf snapshot nodes are associated with subvolumes;
 * interior nodes exist purely for tree structure. Snapshot trees are grouped
 * under \texttt{snapshot\_tree} entries, each recording the root snapshot and
 * the master subvolume.
 *
 * \subsubsection{Key visibility}
 *
 * Four \hyperref[sec:btrees]{btrees} are snapshot-aware: extents, inodes,
 * dirents, and xattrs. Every key in these btrees includes a snapshot ID in its
 * position
 * (\texttt{bpos.snapshot}), so keys from different snapshots coexist in the same
 * btree ordered by (inode, offset, snapshot). When reading from a snapshot, the
 * iterator walks up the snapshot tree: a key is visible if its snapshot ID is an
 * ancestor of (or equal to) the requested snapshot, and no closer ancestor has
 * overwritten it. Deletion within a snapshot inserts a whiteout key that blocks
 * visibility of the ancestor's version without affecting other snapshots.
 *
 * To avoid a linear parent-pointer walk on every lookup, each snapshot node stores
 * a 128-bit ancestor bitmap for O(1) checks when ancestor and descendant are
 * within 128 snapshot IDs of each other, plus a randomized skiplist of three
 * ancestor IDs for O(log $n$) convergence on deeper trees. During early recovery,
 * before this data is validated, queries fall back to a simple parent walk.
 *
 * \subsubsection{Why snapshot IDs descend}
 *
 * New IDs are allocated below every existing one (\texttt{create\_snapids()}),
 * so a node's ID is always lower than its parent's: going up the tree, IDs
 * increase. Three things follow:
 *
 * \begin{itemize}
 *	\item \emph{Visibility filtering is a forward scan.} Keys sort by
 *		\texttt{(inode, offset, snapshot)}, so the ancestors of $S$ sort
 *		after $S$, nearest first: the first key a forward scan finds in an
 *		ancestor of $S$ is the one $S$ sees.
 *
 *	\item \emph{Btree order is a topological sort.} Iterating snapshot keys
 *		in order visits every node before its parent --- the order snapshot
 *		deletion needs - with no sort or graph walk.
 *
 *	\item \emph{Tree shape is checkable from a single key.}
 *		\texttt{bch2\_snapshot\_validate()} checks that children are below
 *		their parent and skiplist entries at or above it with integer
 *		comparisons, without the snapshot table --- so it runs at journal
 *		write time, and a deletion that would leave a bad skiplist is
 *		refused there, not found by a later fsck.
 * \end{itemize}
 *
 * \subsubsection{Snapshot creation}
 *
 * When a snapshot is created, two new snapshot nodes are allocated as children of
 * the source subvolume's current snapshot node. One child becomes the new
 * snapshot's ID; the other replaces the source subvolume's snapshot ID. No keys
 * are copied: both children inherit visibility of all ancestor keys through the
 * snapshot tree. Subsequent writes to either subvolume create new keys tagged with
 * that subvolume's snapshot ID, diverging only where modifications occur.
 *
 * This is fundamentally different from btrfs, which clones entire COW btrees on
 * snapshot. Because bcachefs snapshots share the actual btree keys (not copies),
 * creation is O(1) regardless of filesystem size. Many thousands or millions of
 * snapshots can be created, limited only by disk space.
 *
 * \subsubsection{Snapshot invariants}
 *
 * Four properties hold on a consistent filesystem. They are stated here
 * because the deletion process below depends on them, and because a violation
 * of any one is repaired by a different fsck pass.
 *
 * \begin{description}
 * \item[Key implies inode]\ A key in snapshot $X$ implies a key in the inodes
 *   btree at $X$, for the same inode number. Not merely an inode: a whiteout
 *   also satisfies it. This is what makes an inode's content locatable from
 *   the inodes btree alone, which is how deletion finds keys (below), so it is
 *   the load-bearing one. Enforced by \texttt{bch2\_check\_key\_has\_inode()}
 *   in every content pass.
 *
 * \item[Writes update the inode]\ Writing a key into a snapshot updates that
 *   inode in the same snapshot, which is what maintains the above.
 *
 * \item[Extents align across snapshots]\ Two extents in different snapshots
 *   either occupy exactly the same range or do not overlap at all. Partial
 *   overlap is invalid; the write path splits ancestors' extents so that a
 *   descendant's write lands exactly aligned. Checked by
 *   \texttt{check\_overlapping\_extents}.
 *
 * \item[Subvolumes point at leaves]\ A subvolume's snapshot field always names
 *   a leaf node, never an interior one, and snapshot deletion never repoints a
 *   subvolume --- only snapshot creation does.
 * \end{description}
 *
 * \subsubsection{Snapshot deletion}
 *
 * Deletion is not a single operation. It marks nodes, then reclaims them over
 * one or more runs of the \texttt{delete\_dead\_snapshots} recovery pass, which
 * also runs online. Cost is proportional to the data in the deleted snapshot:
 * the pass has to find and move or drop every key stamped with its id.
 *
 * A node passes through states (\texttt{bch\_snapshot\_state}):
 *
 * \begin{description}
 * \item[\texttt{live}]\ Normal.
 * \item[\texttt{will\_delete}]\ A leaf no longer referenced by a subvolume,
 *   pending reclaim. The subvolume backref is kept: deletion checks it, and a
 *   \texttt{will\_delete} leaf with no subvolume pointing back is invalid.
 * \item[\texttt{no\_keys}]\ An interior node whose keys have all been moved
 *   down. It stays in the tree because removing an interior node means
 *   rewriting depth and skiplist fields across arbitrarily many children,
 *   which cannot be done atomically at runtime. The splice happens at the next
 *   mount, in the single-threaded recovery context, where
 *   \texttt{check\_snapshots} can run afterwards.
 * \item[\texttt{deleted}]\ Spliced out.
 * \end{description}
 *
 * \texttt{delete\_dead\_snapshots} handles dead leaf nodes and redundant
 * interior nodes:
 *
 * \begin{description}
 * \item[Dead]\ No live descendant: a leaf no subvolume points at, or an
 *   interior node whose children are all dead. Nothing that survives sees its
 *   keys, so they're dropped.
 * \item[Redundant]\ An interior node with exactly one live child. That child
 *   still sees the node's keys through inheritance, so they move down to it
 *   instead of being dropped. Where they move is \texttt{live\_child}. With
 *   two live children there's nowhere to collapse into and the node stays ---
 *   delete one child first.
 * \end{description}
 *
 * One scan of the snapshots btree sorts every node into those two cases, in
 * ascending id order. A node's id is always below its parent's (ids are
 * allocated downward), so children are sorted before parents, and that gives
 * two things.
 *
 * A node whose children are all dead is itself dead, and its children are
 * already on the delete list when it's examined - so a dead subtree
 * accumulates bottom-up. It's torn down in that same order, each node
 * childless by its turn because deleting a child splices the parent's pointer
 * to zero. \texttt{bch2\_snapshot\_node\_delete()} refuses a node that still
 * has a child, which is what enforces it.
 *
 * And a node collapsing into a child that's itself collapsing gets that
 * child's destination, already recorded. So \texttt{live\_child} is the
 * terminal of the whole chain, not the next node down, and keys move there in
 * one hop.
 *
 * A pass collects the dying nodes, then:
 *
 * \begin{enumerate}
 * \item \textbf{Content keys.} For each inode with a key in the inodes btree
 *   at a dying snapshot, move or drop that inum's extents, dirents, xattrs and
 *   damage keys. Note the direction: the scan is driven \emph{from} the inodes
 *   btree, and descends into the other btrees only for inums it finds there.
 *   That is the acceleration --- and it is why the key-implies-inode invariant
 *   is load-bearing. A content key whose snapshot has no inodes-btree key is
 *   invisible to this scan and is silently left behind.
 *
 * \item \textbf{Verify.} Before touching the inodes btree, check per-snapshot
 *   accounting for the content btrees. Deleting the inode keys destroys the
 *   index step 1 just navigated by, so anything still accounted at this point
 *   would become unfindable by this pass and every later one. If anything
 *   remains, stop without deleting.
 *
 * \item \textbf{Inode keys.} Now safe to move or drop.
 *
 * \item \textbf{Nodes.} Delete leaves; mark emptied interiors \texttt{no\_keys}.
 *   Both are gated on per-snapshot accounting reading zero --- a node with data
 *   still accounted to it is refused rather than emptied, to prevent data loss.
 * \end{enumerate}
 *
 * If accounting says step 1 didn't evict every key at a dying snapshot id,
 * then there was a key with no inodes-btree key at its snapshot, and the scan
 * never reached it. If that happens, the pass doesn't delete: it schedules
 * \texttt{check\_extents}, \texttt{check\_dirents} or \texttt{check\_xattrs}
 * for the btree that still accounts data. If that pass finds a key with no
 * inodes-btree key at its snapshot, \texttt{bch2\_check\_key\_has\_inode()}
 * inserts one --- an inode or a whiteout. Then the next
 * \texttt{delete\_dead\_snapshots} run reaches the key through it.
 *
 * Keys move \emph{down}, from a dying node to its live descendant, and the
 * inodes btree is always processed last. So the only skew an interrupted run
 * can leave is a content key already moved while its inode has not been: the
 * reverse cannot occur, and checks may rely on that.
 *
 * Collapsing migrates keys down, so an interrupted run leaves them split
 * between the node and its live child; see equivalence classes, below.
 *
 * Progress: \texttt{/sys/fs/bcachefs/<uuid>/snapshot\_delete\_status}. The
 * \texttt{auto\_snapshot\_deletion} option controls whether it runs
 * automatically.
 *
 * \subsubsection{Equivalence classes}
 *
 * If \texttt{delete\_dead\_snapshots} is interrupted partway through
 * collapsing a redundant interior node, its keys are left split: some moved
 * down to the live child, some still at the node. fsck runs before
 * \texttt{delete\_dead\_snapshots}, so it runs on that. It can't wait for the
 * collapse to finish either: if the pass stops at step 2 or 4, the tree stays
 * that way across mounts, and what unblocks it is a repair fsck makes.
 *
 * If fsck compares raw snapshot ids, a split node looks like two snapshots
 * that disagree. \texttt{check\_extents} counts the inode version at the node
 * and the version at the child over keys divided between them, and gets
 * \texttt{i\_sectors} wrong for both. \texttt{check\_overlapping\_extents}
 * reads the two halves as extents in different snapshots, and reports invalid
 * partial overlap.
 *
 * So fsck compares in the collapsed frame.
 * \texttt{bch2\_snapshot\_redundant\_interior()} walks the collapse chain down
 * and returns its terminal: the node the chain is provably collapsing into.
 * Every node on the chain returns the same terminal, and those nodes are one
 * equivalence class. Converting both sides before comparing makes the node and
 * its child the same snapshot, so a half-migrated key is not a false positive.
 * \texttt{inode\_walker} keeps one entry per class, at the terminal, so an
 * inode a repair writes back lands in the destination snapshot;
 * \texttt{snapshots\_seen} keeps its overwrite list in terminals
 * (\texttt{pos\_equiv}).
 *
 * A comparison belongs in the collapsed frame only if what consumes its answer
 * does. \texttt{bch2\_check\_key\_has\_inode()} does not:
 * \texttt{delete\_dead\_snapshot\_keys\_v2()} descends into an inum's content
 * btrees only where the inodes btree has a key at exactly the dying snapshot
 * id, whiteouts included. If that check compared terminals, then a key at the
 * interior node whose inode key exists only at the child would look fine ---
 * and that is the one case that breaks the scan. Those keys were never
 * reported, never repaired and never migrated, and the node could never be
 * emptied. So it compares raw ids, against
 * \texttt{inode\_walker.inode\_snapshots}, which records every id the inodes
 * btree has for that inum.
 *
 * \subsubsection{Space accounting}
 *
 * Space usage is tracked per snapshot via the accounting subsystem. The
 * \texttt{bcachefs subvolume list-snapshots} command shows per-snapshot disk
 * usage attribution (own data vs.\ cumulative including children). Because
 * snapshots share data through COW, the sum of individual snapshot usage will
 * exceed the actual disk usage --- the difference is shared data.
 *
 * \subsubsection{Consistency and self-healing}
 *
 * Several recovery passes validate snapshot consistency:
 *
 * \begin{description}
 * \item[\texttt{check\_snapshots}] Validates the snapshot tree structure:
 *   parent/child links, depth fields, skiplist entries, and ancestor bitmaps.
 *   Detects and repairs orphaned or malformed snapshot nodes.
 * \item[\texttt{check\_subvols}] Validates subvolume entries: ensures each
 *   points to a valid snapshot leaf, root inode exists, and master subvolume
 *   designation is consistent.
 * \item[\texttt{delete\_dead\_snapshots}] Runs the snapshot deletion cleanup
 *   for any snapshots marked for deletion but not yet fully cleaned up.
 * \end{description}
 *
 * These passes can run online. The snapshot deletion thread itself is
 * self-healing: if it is interrupted (crash, reboot), it resumes cleanup on next
 * mount by re-scanning for snapshots still marked for deletion.
 */

use crate::btree::bkey::{bkey_gt, bkey_lt, pos, spos, BkeyI, BkeySC, SizedBkey, POS_MAX, POS_MIN};
use crate::btree::bkey_methods::TriggerOp;
use crate::util::vstructs::{Validated, Whole};
use crate::btree::iter::{is_restart, lockrestart_do, BtreeIter, BtreeIterFlags, BtreeTrans,
                         TransAttempt, TransBkey, UpdateTriggerFlags};
use crate::{bch_err, bkey_fsck_err_on};
use crate::init::error::{id, BkeyValidate};
use crate::c;
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::fs::Fs;
use crate::init::passes;
use crate::snapshots::{check_snapshots, delete, subvolume};
use crate::util::log::{LogLevel, LogMsg};
use crate::types::bits_to_longs;
use crate::util::alloc::{flags::GFP_KERNEL, AllocError, KVVec};
use crate::util::rcu::RcuArray;
use crate::util::ffi::Opaque;
use crate::util::sync::MutexGuard;
use crate::util::Printbuf;
use core::ffi::c_int;
use core::fmt;
use core::sync::atomic::{AtomicU32, Ordering};
use cstruct_macros::c_export;

// ── The table ────────────────────────────────────────────────────────────

/// The in-memory snapshot table: C's array of snapshot_t, indexed by
/// U32_MAX - id. Ids descend, so a table covering a node covers its
/// ancestors.
struct Table<'g>(&'g [c::snapshot_t]);

impl<'g> Table<'g> {
    fn entry(&self, id: u32) -> Option<&'g c::snapshot_t> {
        self.0.get(table_idx(id))
    }

    /// 0 for none.
    fn parent(&self, id: u32) -> u32 {
        self.entry(id).map_or(0, |s| s.parent())
    }

    /// 0s for none.
    fn children(&self, id: u32) -> [u32; 2] {
        self.entry(id).map_or([0, 0], |s| s.children())
    }

    /// For the walks, which only reach nodes the table has: as C's, no check.
    fn walked(&self, id: u32) -> &'g c::snapshot_t {
        self.entry(id).unwrap_or_else(|| panic!("snapshot tree walk: {id} not in the table"))
    }

    fn id_state(&self, id: u32) -> c::snapshot_id_state {
        self.entry(id).map_or(c::snapshot_id_state::SNAPSHOT_ID_empty, |s| s.state())
    }

    /// 0 for none.
    fn tree(&self, id: u32) -> u32 {
        self.entry(id).map_or(0, |s| s.tree())
    }

    fn root(&self, mut id: u32) -> u32 {
        loop {
            match self.parent(id) {
                0      => return id,
                parent => id = parent,
            }
        }
    }

    /// See bch_fs_snapshots::live_descendent().
    fn live_descendent(&self, mut id: u32) -> Result<Option<u32>, bch_errcode> {
        loop {
            let s = self.entry(id).ok_or(bch_errcode::BCH_ERR_invalid_snapshot_node)?;
            if s.state() == c::snapshot_id_state::SNAPSHOT_ID_live {
                return Ok(Some(id));
            }
            // deleted nodes are single-child
            match s.children() {
                [_, c1] if c1 != 0 => return Err(bch_errcode::BCH_ERR_snapshot_multiple_descendents),
                [0, _]             => return Ok(None),
                [c0, _]            => id = c0,
            }
        }
    }

    /// @id's @n'th ancestor, 0 past the root.
    fn nth_parent(&self, mut id: u32, n: u32) -> u32 {
        for _ in 0..n {
            if id == 0 {
                break;
            }
            id = self.parent(id);
        }
        id
    }

    /// By parent pointers alone.
    fn is_ancestor_early(&self, mut id: u32, ancestor: u32) -> bool {
        while id != 0 && id < ancestor {
            id = self.parent(id);
        }
        id == ancestor
    }

    /// The highest of @id's skiplist entries not above @ancestor, else its
    /// parent: 0 if @id isn't in the table.
    fn ancestor_below(&self, id: u32, ancestor: u32) -> u32 {
        let Some(s) = self.entry(id) else { return 0 };
        s.skip().into_iter().rev().find(|&a| a <= ancestor).unwrap_or_else(|| s.parent())
    }

    /// @ancestor's bit in @id's is_ancestor[]: @ancestor is above @id, by
    /// fewer than IS_ANCESTOR_BITMAP.
    fn ancestor_bit(&self, id: u32, ancestor: u32) -> bool {
        self.entry(id).is_some_and(|s| s.is_ancestor_bit((ancestor - id - 1) as usize))
    }

    /// The next node of a preorder walk: left child first, then up to the
    /// nearest right child not yet visited - @depth following it down and
    /// up. 0 when the walk leaves the tree.
    fn tree_next(&self, id: u32, depth: &mut i32) -> u32 {
        let n = self.walked(id).children()[0];
        if n != 0 {
            *depth += 1;
            return n;
        }

        let mut id = id;
        loop {
            let parent = self.parent(id);
            if parent == 0 {
                return 0;
            }
            *depth -= 1;
            let n = self.walked(parent).children()[1];
            if n != 0 && n != id {
                *depth += 1;
                return n;
            }
            id = parent;
        }
    }

    /// @root and every node below it, preorder, with their depths below it.
    fn subtree(&self, root: u32) -> impl Iterator<Item = (u32, i32)> + '_ {
        core::iter::successors((root != 0).then_some((root, 0)), move |&(id, mut depth)| {
            let n = self.tree_next(id, &mut depth);
            (n != 0 && n <= root).then_some((n, depth))
        })
    }

    /// See bch_fs_snapshots::redundant_interior().
    fn redundant_interior(&self, orig: u32) -> Option<u32> {
        let mut id = orig;
        let mut s = self.entry(id)?;

        loop {
            let mut live = s.children().into_iter()
                .filter(|&c| self.id_state(c) == c::snapshot_id_state::SNAPSHOT_ID_live);

            let child = match (live.next(), live.next()) {
                (Some(c), None) => c,
                _ => return (s.state() == c::snapshot_id_state::SNAPSHOT_ID_live && id != orig)
                    .then_some(id),
            };

            id = child;
            s = self.entry(id).expect("live, so in the table");
        }
    }
}

fn table_idx(id: u32) -> usize {
    (u32::MAX - id) as usize
}

/// An entry's fields, each read on its own: one being rewritten can mix old
/// and new - see mark_snapshot().
impl c::snapshot_t {
    fn state(&self) -> c::snapshot_id_state {
        c::snapshot_id_state::from_bits(self.state.load(Ordering::Relaxed))
    }
    fn parent(&self) -> u32        { self.parent.load(Ordering::Relaxed) }
    fn children(&self) -> [u32; 2] { self.children.each_ref().map(|c| c.load(Ordering::Relaxed)) }
    fn depth(&self) -> u32         { self.depth.load(Ordering::Relaxed) }
    fn tree(&self) -> u32          { self.tree.load(Ordering::Relaxed) }
    fn subvol(&self) -> u32        { self.subvol.load(Ordering::Relaxed) }
    fn skip(&self) -> [u32; 3]     { self.skip.each_ref().map(|s| s.load(Ordering::Relaxed)) }

    fn is_ancestor_bit(&self, bit: usize) -> bool {
        let word = self.is_ancestor[bit / usize::BITS as usize].load(Ordering::Relaxed);
        word & (1 << (bit % usize::BITS as usize)) != 0
    }
}

// ── The table's writer ───────────────────────────────────────────────────

/// table_lock, held: the table's write side, and the only way to it.
pub(crate) struct TableWrite<'a> {
    table: &'a RcuArray<c::snapshot_t>,
    _lock: MutexGuard<'a, ()>,
}

impl TableWrite<'_> {
    /// As it stands: it can't grow while this is borrowed.
    fn table(&self) -> Table<'_> {
        // SAFETY: we hold table_lock, and grow only through &mut self
        Table(unsafe { self.table.get_locked() })
    }

    /// Room for @id's entry: as bch2_snapshot_t_mut().
    fn make_room(&mut self, id: u32) -> Result<(), AllocError> {
        // SAFETY: we hold table_lock
        unsafe { self.table.grow_to(table_idx(id) + 1) }
    }
}

/// Bring @k's entry up to date - its node's, or zeroes if it was deleted: for
/// the trigger, for reading and rebuilding the table, and for create_snapids()
/// on a node before its commit.
fn mark_snapshot(fs: &Fs, k: BkeySC<'_>) -> Result<(), BchError> {
    let id = k.k.p.offset as u32;
    let mut w = fs.snapshots.table_write();

    if w.make_room(id).is_err() {
        return fs.throw(bch_errcode::BCH_ERR_ENOMEM_mark_snapshot);
    }

    let table = w.table();
    let e = table.entry(id).expect("just made room for it");
    let store = |a: &AtomicU32, v: u32| a.store(v, Ordering::Relaxed);

    let Some(v) = val(k) else {
        for a in [&e.state, &e.parent, &e.depth, &e.subvol, &e.tree].into_iter()
            .chain(&e.skip).chain(&e.children) {
            store(a, 0);
        }
        for b in &e.is_ancestor {
            b.store(0, Ordering::Relaxed);
        }
        return Ok(());
    };

    let state = v.state_compat();
    let id_state = if state != c::bch_snapshot_state::SNAPSHOT_STATE_no_keys &&
                      state != c::bch_snapshot_state::SNAPSHOT_STATE_deleted {
        c::snapshot_id_state::SNAPSHOT_ID_live
    } else {
        c::snapshot_id_state::SNAPSHOT_ID_deleted
    };

    store(&e.state, id_state.into_bits());
    store(&e.parent, v.parent.get());
    for (a, c) in e.children.iter().zip(v.children) {
        store(a, c.get());
    }
    store(&e.subvol, v.subvol.get());
    store(&e.tree, v.tree.get());
    store(&e.depth, v.depth.get());
    for (a, s) in e.skip.iter().zip(v.skip) {
        store(a, s.get());
    }

    // Readers read the entry under RCU, without table_lock, so one being
    // rewritten here can read as a mix of old and new - and
    // __bch2_snapshot_is_ancestor()'s fast path answers from is_ancestor
    // directly: a torn bitmap is a wrong answer, not a slow one.
    //
    // What makes that safe is when entries change. A new node's is written
    // before anything refers to it. A state change, or a leaf's deletion,
    // changes no node's ancestors, so the bits written are the bits already
    // there. Topology changes - repair, the table rebuild, interior node
    // deletion - happen in recovery with check_snapshots pending, when
    // is_ancestor walks parent pointers instead; CONFIG_BCACHEFS_DEBUG checks
    // every answer against that walk. The exception is an online
    // check_snapshots: its repairs rewrite topology with readers on the fast
    // path.
    //
    // Ids descend, so an ancestor's bit is parent - id - 1; a parent that
    // doesn't descend is damage, and wraps out of range.
    let mut is_ancestor = [0usize; bits_to_longs(c::IS_ANCESTOR_BITMAP as usize)];
    let mut parent = id;
    loop {
        parent = table.parent(parent);
        let bit = parent.wrapping_sub(id).wrapping_sub(1) as usize;
        if parent == 0 || bit >= c::IS_ANCESTOR_BITMAP as usize {
            break;
        }
        is_ancestor[bit / usize::BITS as usize] |= 1 << (bit % usize::BITS as usize);
    }
    for (a, b) in e.is_ancestor.iter().zip(is_ancestor) {
        a.store(b, Ordering::Relaxed);
    }

    if state == c::bch_snapshot_state::SNAPSHOT_STATE_will_delete {
        // Schedule the deleter. This may run as a BTREE_TRIGGER_atomic
        // trigger - btree write locks held, committed to the commit - so the
        // schedule must be ephemeral (no sb_lock) and best-effort: an error
        // here would take the filesystem emergency read-only.
        //
        // In recovery this injects delete_dead_snapshots into the running
        // passes so it runs (in listing order) before the content checks;
        // otherwise it schedules the async runner.
        let mut buf = Printbuf::new();
        let _ = passes::run_explicit(fs, &mut buf, c::bch_recovery_pass::BCH_RECOVERY_PASS_delete_dead_snapshots,
                                     c::bch_run_recovery_pass_flags::RUN_RECOVERY_PASS_ephemeral);
    }
    Ok(())
}

// ── Queries, and accounting ──────────────────────────────────────────────

impl c::bch_fs_snapshots {
    /// The filesystem these are in, whose errors they throw.
    fn fs(&self) -> &Fs {
        unsafe {
            Fs::borrow_raw((self as *const Self).byte_sub(core::mem::offset_of!(c::bch_fs, snapshots))
                           .cast::<c::bch_fs>().cast_mut())
        }
    }

    /// @f on the table, in a read-side section for as long as it runs.
    fn with_table<R>(&self, f: impl FnOnce(&Table<'_>) -> R) -> R {
        self.table.with(|s| f(&Table(s)))
    }

    pub fn parent(&self, id: u32) -> Option<u32> {
        match self.with_table(|t| t.parent(id)) {
            0      => None,
            parent => Some(parent),
        }
    }

    /// If @id is a redundant interior node, the live snapshot it collapses
    /// into.
    ///
    /// A redundant interior node: a live interior node with exactly one child
    /// that isn't being deleted. delete_dead_snapshots will collapse it into
    /// that child, migrating its keys down; interrupted, that leaves
    /// half-migrated keys split between the node and the child.
    ///
    /// fsck uses this to check such a node's keys/paths in the live child's
    /// view - the state the subtree is provably collapsing into - so a
    /// half-migrated key isn't a false positive. On a healthy fs there are
    /// none: taking a snapshot always makes a 2-child split, so a single-child
    /// interior only exists as the residue of a deletion.
    pub fn redundant_interior(&self, id: u32) -> Option<u32> {
        self.with_table(|t| t.redundant_interior(id))
    }

    /// By parent pointers, before the ancestor bitmaps are built.
    pub fn is_ancestor_early(&self, id: u32, ancestor: u32) -> bool {
        self.with_table(|t| t.is_ancestor_early(id, ancestor))
    }

    pub fn has_children(&self, id: u32) -> bool {
        self.with_table(|t| t.children(id) != [0, 0])
    }

    /// 0 if there's none, or no such snapshot.
    pub fn first_child(&self, id: u32) -> u32 {
        self.with_table(|t| t.children(id)[0])
    }

    /// An error if there's no such snapshot.
    pub fn is_leaf(&self, id: u32) -> Result<bool, BchError> {
        match self.with_table(|t| t.entry(id).map(|s| s.children()[0])) {
            Some(child) => Ok(child == 0),
            None        => self.fs().throw(bch_errcode::BCH_ERR_invalid_snapshot_node),
        }
    }

    /// 0 if there's no such snapshot.
    pub fn tree(&self, id: u32) -> u32 {
        self.with_table(|t| t.tree(id))
    }

    /// By table entry, whatever state the nodes are in.
    pub fn same_tree(&self, id1: u32, id2: u32) -> bool {
        id1 == id2 || self.with_table(|t| match (t.entry(id1), t.entry(id2)) {
            (Some(s1), Some(s2)) => s1.tree() == s2.tree(),
            _                    => false,
        })
    }

    pub fn root(&self, id: u32) -> u32 {
        self.with_table(|t| t.root(id))
    }

    pub fn id_state(&self, id: u32) -> c::snapshot_id_state {
        self.with_table(|t| t.id_state(id))
    }

    /// @id is a live node: as bch2_snapshot_exists().
    pub fn exists(&self, id: u32) -> bool {
        self.id_state(id) == c::snapshot_id_state::SNAPSHOT_ID_live
    }

    /// Of a child of @parent; 0 for no parent. Panics if @parent isn't in the
    /// table, rather than have repair write a wrong depth.
    pub fn depth(&self, parent: u32) -> u32 {
        if parent == 0 {
            return 0;
        }
        let depth = self.with_table(|t| t.entry(parent)
            .unwrap_or_else(|| panic!("snapshot depth: parent {parent} not in the table"))
            .depth());
        // A parent at the maximum is damaged, and check_depth()'s to repair
        depth.saturating_add(1)
    }

    /// For a skiplist entry: a random ancestor of @id, @id itself for a root,
    /// 0 for 0. Panics if @id isn't in the table, as depth() does.
    pub fn skiplist_get(&self, id: u32) -> u32 {
        if id == 0 {
            return 0;
        }
        self.with_table(|t| {
            let s = t.entry(id)
                .unwrap_or_else(|| panic!("snapshot skiplist: {id} not in the table"));

            if s.parent() == 0 {
                return id;
            }
            // depth 0 under a parent is damage, check_depth()'s: C's
            // get_random_u32_below(0) is any u32, and the walk runs off the root
            let n = match s.depth() {
                0     => crate::util::kernel::random_u64() as u32,
                depth => crate::util::kernel::random_u64_below(depth as u64) as u32,
            };
            t.nth_parent(id, n)
        })
    }

    /// The lowest numbered subvolume in @root's subtree.
    pub fn oldest_subvol(&self, root: u32) -> Option<u32> {
        self.with_table(|t| t.subtree(root)
            .map(|(id, _)| t.walked(id).subvol())
            .filter(|&s| s != 0)
            .min())
    }

    /// The unique live node claiming an edge with @id - as its parent if
    /// @parent, else as its child - other than the nodes @s, @id's key,
    /// already points at (intact edges, stale entries for the disputed
    /// pointer, cycles). None if there's none, or more than one: fsck's edge
    /// repair must not guess.
    ///
    /// Live nodes only: a tombstone's child pointer is a splice breadcrumb,
    /// not a claim.
    pub fn table_find_edge(&self, s: &c::Snapshot, id: u32, parent: bool) -> Option<u32> {
        let known = [s.parent.get(), s.children[0].get(), s.children[1].get()];
        self.with_table(|t| {
            let mut claims = t.0.iter().enumerate()
                .filter(|(_, n)| n.state() == c::snapshot_id_state::SNAPSHOT_ID_live)
                .filter(|(_, n)| if parent { n.children().contains(&id) } else { n.parent() == id })
                .map(|(idx, _)| u32::MAX - idx as u32)
                .filter(|n_id| !known.contains(n_id));

            match (claims.next(), claims.next()) {
                (Some(n), None) => Some(n),
                _               => None,
            }
        })
    }

    /// For node @id, about to be created.
    pub fn table_make_room(&self, id: u32) -> Result<(), BchError> {
        self.table_write().make_room(id)
            .map_err(|_| self.fs().err(bch_errcode::BCH_ERR_ENOMEM_mark_snapshot))
    }

    pub(crate) fn table_write(&self) -> TableWrite<'_> {
        TableWrite { table: &self.table, _lock: self.table_lock.lock() }
    }

    /// Have the table rebuilt from the btree, after repairs it doesn't track:
    /// see table_rebuild_if_needed().
    pub fn set_need_table_rebuild(&self) {
        self.need_table_rebuild.store(true, Ordering::Relaxed);
    }

    /// @id, or the live snapshot it collapses into if it's dead. An error if the
    /// tree is damaged on the way.
    ///
    /// We can have partially deleted nodes in the no_keys state: part of the
    /// tree - interior nodes can't be removed from it at runtime - but every
    /// key at that id has been removed and no new ones can be made. This walks
    /// to the single live descendant to use instead: None if the subtree has
    /// been fully deleted and there is none (a stray key in such a node has no
    /// live home and should just be dropped).
    ///
    /// A missing node mid-walk means a deleted interior node's child pointer
    /// is dangling - the tree is damaged - and is an error rather than
    /// conflated with None, so the caller doesn't drop a key a live
    /// descendant should still see.
    pub fn live_descendent(&self, id: u32) -> Result<Option<u32>, BchError> {
        self.with_table(|t| t.live_descendent(id)).or_else(|e| self.fs().throw(e))
    }

    /// What's accounted to snapshot @id.
    ///
    /// In-mem: bch2_accounting_is_mem() covers everything but
    /// BCH_DISK_ACCOUNTING_inum, so the counters are current as deltas are
    /// applied. No btree read, no transaction, and no write buffer flush to
    /// make the values trustworthy.
    ///
    /// The key count catches metadata-only stranding (dirents, xattrs, empty
    /// inodes) that the sectors counter can't see. It's only trusted once
    /// check_allocations has rebuilt it (scheduled by the
    /// per_dev_fragmentation_lru upgrade); before that it reads as zero, and
    /// only sectors count.
    pub fn accounting(&self, id: u32) -> SnapshotAccounting {
        let fs = self.fs();
        let trust_keys = fs.version_upgrade_complete().0 >=
            c::bcachefs_metadata_version::bcachefs_metadata_version_per_dev_fragmentation_lru.0;

        let mut btrees = crate::btrees_in(crate::BTREE_HAS_SNAPSHOTS_MASK);
        SnapshotAccounting(core::array::from_fn(|_| {
            let btree = btrees.next().expect("NR_SNAPSHOT_BTREES is the mask's popcount");

            let mut v = [0u64; 3];
            crate::accounting::mem_read(fs, &c::disk_accounting_pos::from_arm(
                    c::bch_acct_snapshot { id, btree: btree.0 }), &mut v);

            BtreeAccounting {
                btree,
                keys:      if trust_keys { v[0] } else { 0 },
                key_bytes: if trust_keys { v[1] } else { 0 },
                sectors:   v[2],
            }
        }))
    }
}

const NR_SNAPSHOT_BTREES: usize = crate::BTREE_HAS_SNAPSHOTS_MASK.count_ones() as usize;

#[derive(Clone, Copy)]
struct BtreeAccounting {
    btree:     c::btree_id,
    keys:      u64,
    key_bytes: u64,
    sectors:   u64,
}

impl BtreeAccounting {
    fn is_empty(&self) -> bool {
        self.keys == 0 && self.sectors == 0
    }
}

/// Keys and sectors accounted to a snapshot, per snapshotted btree. Displays
/// as the per-btree breakdown, a line for each btree with anything in it.
pub struct SnapshotAccounting([BtreeAccounting; NR_SNAPSHOT_BTREES]);

impl SnapshotAccounting {
    pub fn keys(&self) -> u64    { self.0.iter().map(|b| b.keys).sum() }
    pub fn sectors(&self) -> u64 { self.0.iter().map(|b| b.sectors).sum() }

    pub fn is_empty(&self) -> bool {
        self.0.iter().all(BtreeAccounting::is_empty)
    }

    /// A bit per btree with anything accounted.
    pub fn btrees(&self) -> u64 {
        self.0.iter().filter(|b| !b.is_empty()).fold(0, |m, b| m | 1u64 << b.btree.0)
    }
}

impl fmt::Display for SnapshotAccounting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0.iter().filter(|b| !b.is_empty()) {
            write!(f, "\n  {}: {} keys ({} bytes), {} sectors", b.btree, b.keys, b.key_bytes, b.sectors)?;
        }
        Ok(())
    }
}

// ── Ancestry ─────────────────────────────────────────────────────────────

/// Whether @id is @ancestor or one of its descendants.
pub fn is_ancestor(t: &TransAttempt<'_, '_>, id: u32, ancestor: u32) -> bool {
    debug_assert!(id != 0 && ancestor != 0);
    id == ancestor || is_ancestor_table(t, id, ancestor)
}

/// By the table: up the skiplists to within IS_ANCESTOR_BITMAP of
/// @ancestor, then its bit - or the parent walk, while check_snapshots has
/// yet to vouch for what the skiplists and bitmaps were built from.
fn is_ancestor_table(t: &TransAttempt<'_, '_>, orig_id: u32, ancestor: u32) -> bool {
    let fs = t.fs();
    let bitmap = c::IS_ANCESTOR_BITMAP;

    if passes::will_run(fs, c::bch_recovery_pass::BCH_RECOVERY_PASS_check_snapshots) {
        return fs.snapshots.is_ancestor_early(orig_id, ancestor);
    }

    let ret = fs.snapshots.with_table(|table| {
        let mut id = orig_id;
        if ancestor >= bitmap {
            while id != 0 && id < ancestor - bitmap {
                id = table.ancestor_below(id, ancestor);
            }
        }

        if id != 0 && id < ancestor {
            table.ancestor_bit(id, ancestor)
        } else {
            id == ancestor
        }
    });

    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    is_ancestor_debug(t, orig_id, ancestor, ret);

    ret
}

/// Panic if the fast path disagrees with the parent walk - with both walks,
/// and the btree's, to show which of them is wrong.
#[cfg(CONFIG_BCACHEFS_DEBUG)]
#[cold]
fn is_ancestor_debug(t: &TransAttempt<'_, '_>, id: u32, ancestor: u32, fastpath: bool) {
    let fs = t.fs();
    let slowpath = fs.snapshots.is_ancestor_early(id, ancestor);
    if fastpath == slowpath {
        return;
    }

    let mut buf = Printbuf::new();
    let _ = writeln!(buf, "is_ancestor({id}, {ancestor}): fastpath={} slowpath={}",
                     fastpath as u32, slowpath as u32);
    fs.snapshots.with_table(|table| {
        let _ = write!(buf, "  fastpath: {id}");
        let mut i = id;
        while i != 0 && i < ancestor.wrapping_sub(c::IS_ANCESTOR_BITMAP) {
            i = table.ancestor_below(i, ancestor);
            let _ = write!(buf, " -> {i}");
        }
        if i != 0 && i < ancestor {
            let _ = write!(buf, " bitmap[{}]={}", ancestor - i - 1, table.ancestor_bit(i, ancestor) as u32);
        }
        let _ = writeln!(buf);

        let _ = write!(buf, "  slowpath: {id}");
        let mut i = id;
        while i != 0 && i < ancestor {
            i = table.parent(i);
            let _ = write!(buf, " -> {i}");
        }
        let _ = writeln!(buf);
    });

    let _ = write!(buf, "  btree:    {id}");
    let mut i = id;
    while i != 0 && i < ancestor {
        match lookup_key(t, i) {
            Ok(s) => {
                i = s.v().parent.get();
                let _ = write!(buf, " -> {i}");
            }
            Err(e) => {
                let _ = write!(buf, " (lookup error {})", -e.raw());
                break;
            }
        }
    }
    let _ = writeln!(buf);

    panic!("{}", buf.as_str());
}

#[c_export]
pub fn __bch2_snapshot_is_ancestor(trans: &Opaque<c::btree_trans>, id: u32, ancestor: u32) -> bool {
    let trans = BtreeTrans::from_c(trans);
    is_ancestor_table(&unsafe { trans.attempt_in_progress() }, id, ancestor)
}

#[c_export]
pub fn bch2_snapshot_skiplist_get(c: &Opaque<c::bch_fs>, id: u32) -> u32 {
    Fs::from_c(c).snapshots.skiplist_get(id)
}

// ── Snapshot node keys ───────────────────────────────────────────────────

/// Zero padded past the fields an older key lacks.
pub fn val(k: BkeySC<'_>) -> Option<c::Snapshot> {
    c::Snapshot::val_copy_pad(k)
}

/// The state codeword nearest @v and its Hamming distance - the first, on a
/// tie. Codewords are >= 14 bits apart, so within 6 bits the nearest is the
/// original.
pub fn state_nearest(v: u32) -> (c::bch_snapshot_state, u32) {
    let mut best = (c::bch_snapshot_state::SNAPSHOT_STATE_live, 33);
    for s in SNAPSHOT_STATES {
        let dist = (v ^ s.0).count_ones();
        if dist < best.1 {
            best = (s, dist);
        }
    }
    best
}

const SNAPSHOT_STATES: [c::bch_snapshot_state; 4] = [
    c::bch_snapshot_state::SNAPSHOT_STATE_live,
    c::bch_snapshot_state::SNAPSHOT_STATE_will_delete,
    c::bch_snapshot_state::SNAPSHOT_STATE_no_keys,
    c::bch_snapshot_state::SNAPSHOT_STATE_deleted,
];

/// Any snapshot node, in place or whole - the struct reaches these too.
impl c::Snapshot<Validated> {
    pub fn parent(&self) -> u32       { self.parent.get() }
    pub fn children(&self) -> [u32; 2] { self.children.map(|c| c.get()) }
    pub fn subvol(&self) -> u32       { self.subvol.get() }
    pub fn tree(&self) -> u32         { self.tree.get() }

    /// None if the field is 0 - the key predates it - or damaged.
    pub fn state_field(&self) -> Option<c::bch_snapshot_state> {
        state_field(self.state().get())
    }

    /// The node's state, on any key: zero isn't a valid state, so a zero
    /// field means the key predates it, and the old flag bits are
    /// authoritative.
    pub fn state_compat(&self) -> c::bch_snapshot_state {
        match self.state().get() {
            0 => self.state_from_flags(),
            s => c::bch_snapshot_state(s),
        }
    }

    /// The state the legacy flags imply.
    pub fn state_from_flags(&self) -> c::bch_snapshot_state {
        if self.deleted_obsolete() {
            c::bch_snapshot_state::SNAPSHOT_STATE_deleted
        } else if self.no_keys_obsolete() {
            c::bch_snapshot_state::SNAPSHOT_STATE_no_keys
        } else if self.will_delete_obsolete() {
            c::bch_snapshot_state::SNAPSHOT_STATE_will_delete
        } else {
            c::bch_snapshot_state::SNAPSHOT_STATE_live
        }
    }

    /// From the legacy flags if the key predates the state field; None if the
    /// field is damaged.
    pub fn state_decoded(&self) -> Option<c::bch_snapshot_state> {
        if self.state() == 0 {
            Some(self.state_from_flags())
        } else {
            self.state_field()
        }
    }
}

/// The struct: a field an older key lacks, to set.
impl c::Snapshot<Whole> {
    /// And the legacy flags to match - SUBVOL_OBSOLETE from @subvol, so set
    /// that first.
    pub fn set_state(&mut self, state: c::bch_snapshot_state) {
        use c::bch_snapshot_state as S;

        let flags = match state {
            S::SNAPSHOT_STATE_live        => Some((false, self.subvol.get() != 0, false, false)),
            S::SNAPSHOT_STATE_will_delete => Some((true,  false, false, false)),
            S::SNAPSHOT_STATE_no_keys     => Some((false, false, false, true)),
            S::SNAPSHOT_STATE_deleted     => Some((false, false, true,  false)),
            _                             => None,
        };
        if let Some((will_delete, subvol, deleted, no_keys)) = flags {
            self.set_will_delete_obsolete(will_delete);
            self.set_subvol_obsolete(subvol);
            self.set_deleted_obsolete(deleted);
            self.set_no_keys_obsolete(no_keys);
        }
        self.state = state.0.into();
    }
}

#[c_export]
pub fn bch2_snapshot_state_set(s: &mut c::Snapshot, state: c::bch_snapshot_state) {
    s.set_state(state)
}

impl fmt::Display for c::bch_snapshot_state {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = crate::snapshot_states::SNAPSHOT_STATE_VALUES.iter()
            .find(|(_, v)| *v == self.0 as u64)
            .map_or("(invalid state)", |(n, _)| n);
        f.write_str(name)
    }
}

impl fmt::Display for c::Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "parent {:>10} children {:>10} {:>10} subvol {} tree {}",
               self.parent.get(), self.children[0].get(), self.children[1].get(),
               self.subvol.get(), self.tree.get())?;
        write!(f, " depth {} skiplist {} {} {}",
               self.depth.get(), self.skip[0].get(), self.skip[1].get(), self.skip[2].get())?;

        let flags = self.flags.get();
        let obsolete = ObsoleteFlags(flags);
        let state = self.state.get();

        if state == 0 {
            // Not upgraded: no state field, the obsolete flags are the truth
            write!(f, " {} (not upgraded, flags {flags:#x}:{obsolete})", self.state_from_flags())
        } else if state_field(state).is_none() {
            write!(f, " state {state:#x} invalid (flags {flags:#x}:{obsolete})")
        } else {
            let state = c::bch_snapshot_state(state);
            write!(f, " {state}")?;

            // The obsolete flags are dual-written shadows of the state field
            // and subvol pointer (set_state()); recompute what the dual-write
            // would produce and print only divergence:
            let mut expect = *self;
            expect.set_state(state);

            for (name, is, should) in [
                ("will_delete_obsolete", self.will_delete_obsolete(), expect.will_delete_obsolete()),
                ("subvol_obsolete",      self.subvol_obsolete(),      expect.subvol_obsolete()),
                ("deleted_obsolete",     self.deleted_obsolete(),     expect.deleted_obsolete()),
                ("no_keys_obsolete",     self.no_keys_obsolete(),     expect.no_keys_obsolete()),
            ] {
                if is != should {
                    write!(f, " {name}={}", is as u32)?;
                }
            }
            Ok(())
        }
    }
}

/// The obsolete flag bits by name, comma separated: prt_bitflags()'s.
struct ObsoleteFlags(u32);

impl fmt::Display for ObsoleteFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const NAMES: [&str; 4] = ["will_delete", "subvol", "deleted", "no_keys"];

        // Lowest bit first, stopping at the first bit without a name
        let mut flags = self.0;
        let mut first = true;
        while flags != 0 {
            let bit = flags.trailing_zeros() as usize;
            let Some(name) = NAMES.get(bit) else { break };
            if !first {
                f.write_str(",")?;
            }
            first = false;
            f.write_str(name)?;
            flags ^= 1 << bit;
        }
        Ok(())
    }
}

fn state_field(state: u32) -> Option<c::bch_snapshot_state> {
    SNAPSHOT_STATES.into_iter().find(|&s| s.0 == state)
}

/// Whether snapshot key @k is long enough to have all of @field: bkey_has_field().
macro_rules! has_field {
    ($k:expr, $field:ident) => {
        $k.val_bytes().len() >=
            core::mem::offset_of!(c::Snapshot, repr.$field) +
            core::mem::size_of_val(&c::Snapshot::default().$field)
    };
}

fn validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
    let k = v.k;
    let s = val(k).expect("a snapshot");
    let id = k.k.p.offset;

    bkey_fsck_err_on!(v, bkey_gt(k.k.p, pos(0, u32::MAX as u64)) || bkey_lt(k.k.p, pos(0, 1)),
                      id::snapshot_pos_bad, "bad pos")?;

    let parent = s.parent.get();
    bkey_fsck_err_on!(v, parent != 0 && parent as u64 <= id,
                      id::snapshot_parent_bad, "bad parent node ({parent} <= {id})")?;

    let children = s.children.map(|c| c.get());
    bkey_fsck_err_on!(v, children[0] < children[1],
                      id::snapshot_children_not_normalized, "children not normalized")?;
    bkey_fsck_err_on!(v, children[0] != 0 && children[0] == children[1],
                      id::snapshot_child_duplicate, "duplicate child nodes")?;
    for c in children {
        bkey_fsck_err_on!(v, c as u64 >= id,
                          id::snapshot_child_bad, "bad child node ({c} >= {id})")?;
    }

    if has_field!(k, skip) {
        let skip = s.skip.map(|s| s.get());
        bkey_fsck_err_on!(v, skip[0] > skip[1] || skip[1] > skip[2],
                          id::snapshot_skiplist_not_normalized, "skiplist not normalized")?;
        for i in skip {
            bkey_fsck_err_on!(v, i != 0 && i < parent,
                              id::snapshot_skiplist_bad, "bad skiplist node {i}")?;
        }
    }

    if has_field!(k, pad) {
        bkey_fsck_err_on!(v, s.pad.get() != 0, id::snapshot_pad_nonzero, "reserved pad field nonzero")?;
    }

    // Commit-only checks - defense in depth: catch the buggy writer in the
    // act, with a backtrace. Never applied to existing keys (the invalid-bkey
    // machinery's remedy is dropping the key; on-disk violations are fsck's
    // to handle, gently). Zero state is legal - pre-upgrade keys are
    // rewritten with only their other fields touched:
    if v.from.from() == c::bkey_validate_from::BKEY_VALIDATE_commit &&
       v.fs.opts().no_commit_validate == 0 {
        let state = if has_field!(k, state) { s.state.get() } else { 0 };

        bkey_fsck_err_on!(v, state != 0 && state_field(state).is_none(),
                          id::snapshot_state_bad, "invalid state {state:#x}")?;

        // A subvol backref is only legal on a live or will_delete leaf:
        // subvolumes only reference leaves, and no_keys/deleted nodes were
        // interior when they were emptied - a dead leaf is deleted outright;
        // the no_keys parking state exists only because an interior node
        // can't be removed from the tree at runtime. will_delete retains the
        // backref: it points at the subvolume's tombstone, which the deletion
        // path checks (check_should_delete_leaf()).
        let st = c::bch_snapshot_state(state);
        bkey_fsck_err_on!(v, s.subvol.get() != 0 &&
                          (children[0] != 0 ||
                           (state != 0 &&
                            st != c::bch_snapshot_state::SNAPSHOT_STATE_live &&
                            st != c::bch_snapshot_state::SNAPSHOT_STATE_will_delete)),
                          id::snapshot_should_not_have_subvol,
                          "snapshot with subvol must be a live or will_delete leaf")?;
    }
    Ok(())
}

#[c_export]
pub fn bch2_snapshot_validate(
    c:    &Opaque<c::bch_fs>,
    k:    BkeySC<'_>,
    from: &c::bkey_validate_context,
) -> c_int {
    ret_to_c(validate(&BkeyValidate { fs: Fs::from_c(c), k, from }))
}

#[cold]
#[c_export]
pub fn bch2_snapshot_key_to_text(out: &mut Printbuf, _c: &Opaque<c::bch_fs>, k: BkeySC<'_>) {
    write!(out, "{}", val(k).expect("a snapshot"));
}

fn trigger(t: &TransAttempt<'_, '_>, op: &TriggerOp<'_>) -> Result<(), BchError> {
    if op.flags.contains(UpdateTriggerFlags::TRANSACTIONAL) {
        passes::clear_btree_clean(t.fs(), c::btree_id::snapshots);
    }
    if op.flags.contains(UpdateTriggerFlags::ATOMIC) {
        mark_snapshot(t.fs(), op.new.as_sc())?;
    }
    Ok(())
}

crate::key_trigger!(bch2_snapshot_trigger => trigger);

// ── snapshot_tree keys ───────────────────────────────────────────────────
//
// Keys in BTREE_ID_snapshot_trees identify a whole tree of snapshot nodes;
// they exist to provide a stable identifier for the whole lifetime of a
// snapshot tree.

fn tree_validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
    let p = v.k.k.p;
    bkey_fsck_err_on!(v, bkey_gt(p, pos(0, u32::MAX as u64)) || bkey_lt(p, pos(0, 1)),
                      id::snapshot_tree_pos_bad, "bad pos")?;
    Ok(())
}

#[c_export]
pub fn bch2_snapshot_tree_validate(
    c:    &Opaque<c::bch_fs>,
    k:    BkeySC<'_>,
    from: &c::bkey_validate_context,
) -> c_int {
    ret_to_c(tree_validate(&BkeyValidate { fs: Fs::from_c(c), k, from }))
}

#[cold]
#[c_export]
pub fn bch2_snapshot_tree_to_text(out: &mut Printbuf, _c: &Opaque<c::bch_fs>, k: BkeySC<'_>) {
    let t = c::SnapshotTree::val_copy_pad(k).expect("a snapshot_tree");
    write!(out, "subvol {} root snapshot {}", t.master_subvol.get(), t.root_snapshot.get());
}

pub fn tree_lookup(t: &TransAttempt<'_, '_>, id: u32) -> Result<c::SnapshotTree, BchError> {
    t.bkey_get_val::<c::SnapshotTree>(c::btree_id::snapshot_trees, pos(0, id as u64),
                                      BtreeIterFlags::empty())
        .map_err(|e| if e.matches(c::ENOENT) {
            t.fs().err(bch_errcode::BCH_ERR_ENOENT_snapshot_tree)
        } else {
            e
        })
}

#[c_export]
pub fn bch2_snapshot_tree_lookup(trans: &Opaque<c::btree_trans>, id: u32, s: &mut c::SnapshotTree) -> c_int {
    let trans = BtreeTrans::from_c(trans);
    // SAFETY: C looks up in the attempt it's in
    match tree_lookup(&unsafe { trans.attempt_in_progress() }, id) {
        Ok(v)  => { *s = v; 0 }
        Err(e) => ret_to_c(Err(e)),
    }
}

/// At an unused id, queued for the commit, for the caller to fill in.
pub fn tree_create<'a, 't>(t: &TransAttempt<'a, 't>)
    -> Result<TransBkey<'a, 't, c::SnapshotTree>, BchError>
{
    let mut iter = BtreeIter::uninit();
    match t.bkey_get_empty_slot(&mut iter, c::btree_id::snapshot_trees, POS_MIN,
                                pos(0, u32::MAX as u64)) {
        Err(e) if e.matches(bch_errcode::BCH_ERR_ENOSPC_btree_slot) =>
            return t.fs().throw(bch_errcode::BCH_ERR_ENOSPC_snapshot_tree),
        r => r?,
    }

    let mut k = t.bkey_alloc_typed::<c::SnapshotTree>()?;
    k.k_mut().p = iter.pos();
    t.update(&iter, &k, UpdateTriggerFlags::empty())?;
    Ok(k)
}

// ── Node creation ────────────────────────────────────────────────────────

/// @new_snapids.len() new nodes under @parent, in @tree, each with its
/// subvolume from @subvols: at the free ids below the lowest in use - ids
/// descend - and in the table already, before the commit, for what the rest
/// of the transaction asks about them.
fn create_snapids(
    t:           &TransAttempt<'_, '_>,
    parent:      u32,
    tree:        u32,
    new_snapids: &mut [u32],
    subvols:     &[u32],
) -> Result<(), BchError> {
    let fs = t.fs();
    let depth = fs.snapshots.depth(parent);

    let mut iter = BtreeIter::new(t, c::btree_id::snapshots, POS_MIN, BtreeIterFlags::INTENT);
    iter.peek(t)?;

    for (new_id, &subvol) in new_snapids.iter_mut().zip(subvols) {
        let p = match iter.prev_slot(t)? {
            Some(k) if k.k.p.offset != 0 => k.k.p,
            _ => return fs.throw(bch_errcode::BCH_ERR_ENOSPC_snapshot_create),
        };

        let mut n = t.bkey_alloc_typed::<c::Snapshot>()?;
        n.k_mut().p = p;

        let v = n.v_mut();
        v.flags    = 0u32.into();
        v.parent   = parent.into();
        v.subvol   = subvol.into();
        v.tree     = tree.into();
        v.depth    = depth.into();
        v.btime.lo = fs.current_time().into();
        v.btime.hi = 0u64.into();

        let mut skip: [u32; 3] = core::array::from_fn(|_| fs.snapshots.skiplist_get(parent));
        skip.sort_unstable();
        v.skip = skip.map(Into::into);

        v.set_state(c::bch_snapshot_state::SNAPSHOT_STATE_live);

        t.update(&iter, &n, UpdateTriggerFlags::empty())?;
        mark_snapshot(fs, BkeySC::from(n.k_i()))?;

        *new_id = p.offset as u32;
    }
    Ok(())
}

/// Two new nodes, children of the existing @parent - a snapshot.
fn node_create_children(
    t:           &TransAttempt<'_, '_>,
    parent:      u32,
    new_snapids: &mut [u32],
    subvols:     &[u32],
) -> Result<(), BchError> {
    let fs = t.fs();

    let mut n_parent: TransBkey<'_, '_, c::Snapshot> =
        match t.bkey_get_mut(c::btree_id::snapshots, pos(0, parent as u64),
                             BtreeIterFlags::empty(), UpdateTriggerFlags::empty()) {
            Err(e) if e.matches(c::ENOENT) => {
                bch_err!(fs, "snapshot {parent} not found");
                return Err(e);
            }
            r => r?,
        };

    if n_parent.v().children() != [0, 0] {
        bch_err!(fs, "Trying to add child snapshot nodes to parent that already has children");
        return fs.throw(bch_errcode::BCH_ERR_EINVAL_snapshot_parent_already_has_children);
    }

    create_snapids(t, parent, n_parent.v().tree(), new_snapids, subvols)?;

    let v = n_parent.v_mut();
    v.children = [new_snapids[0].into(), new_snapids[1].into()];
    v.subvol   = 0u32.into();
    v.set_state(c::bch_snapshot_state::SNAPSHOT_STATE_live);
    Ok(())
}

/// One new node, the root of a new tree.
fn node_create_tree(t: &TransAttempt<'_, '_>, new_snapids: &mut [u32], subvols: &[u32])
    -> Result<(), BchError>
{
    let mut n_tree = tree_create(t)?;

    create_snapids(t, 0, n_tree.k().p.offset as u32, new_snapids, subvols)?;

    let v = n_tree.v_mut();
    v.master_subvol = subvols[0].into();
    v.root_snapshot = new_snapids[0].into();
    Ok(())
}

/// New snapshot nodes, one for each of @subvols: two children of @parent, or
/// with @parent 0 the root of a new tree. Their ids go in @new_snapids.
pub fn node_create(t: &TransAttempt<'_, '_>, parent: u32, new_snapids: &mut [u32], subvols: &[u32])
    -> Result<(), BchError>
{
    if new_snapids.len() != subvols.len() {
        return t.fs().throw(bch_errcode::BCH_ERR_invalid_snapshot_node);
    }
    assert_eq!(parent == 0, new_snapids.len() == 1);
    assert_eq!(parent != 0, new_snapids.len() == 2);

    if parent != 0 {
        node_create_children(t, parent, new_snapids, subvols)
    } else {
        node_create_tree(t, new_snapids, subvols)
    }
}

// ── Reading the table ────────────────────────────────────────────────────

/// table_rebuild(), if repairs set need_table_rebuild.
pub fn table_rebuild_if_needed(trans: &BtreeTrans<'_>) -> Result<(), BchError> {
    if !trans.fs().snapshots.need_table_rebuild.load(Ordering::Relaxed) {
        return Ok(());
    }
    table_rebuild(trans)
}

/// A topology repair rewrote a parent pointer: mark_snapshot recomputed that
/// node's table entry, but descendants' is_ancestor bitmaps were built under
/// the old topology. Re-mark everything (reverse: ancestors first). Ancestor
/// queries during check_snapshots itself use the parent-walking _early
/// variants, so the stale window ends before anything trusts the bitmaps.
fn table_rebuild(trans: &BtreeTrans<'_>) -> Result<(), BchError> {
    let fs = trans.fs();
    let mut iter = BtreeIter::new(trans, c::btree_id::snapshots, POS_MAX, BtreeIterFlags::empty());
    iter.for_each_reverse(trans, POS_MIN, |_, _, k| mark_snapshot(fs, k))?;

    fs.snapshots.need_table_rebuild.store(false, Ordering::Relaxed);
    Ok(())
}

/// The table, from the btree - and the deletions it finds to schedule: the
/// snapshots_read recovery pass.
fn snapshots_read(fs: &Fs) -> Result<(), BchError> {
    // It's important that we check if we need to reconstruct snapshots before
    // going RW, so we mark that pass as required in the superblock -
    // otherwise, we could end up deleting keys with missing snapshot nodes
    // instead
    assert!(fs.flag(c::bch_fs_flags::BCH_FS_new_fs) || !fs.flag(c::bch_fs_flags::BCH_FS_may_go_rw),
            "snapshots_read after going rw");

    // Initializing the is_ancestor bitmaps requires ancestors to already be
    // initialized - so mark in reverse:
    let trans = crate::btree_trans!(fs);
    let mut nr_empty_interior = 0;
    let mut iter = BtreeIter::new(&trans, c::btree_id::snapshots, POS_MAX, BtreeIterFlags::empty());
    iter.for_each_reverse(&trans, POS_MIN, |t, _, k| {
        mark_snapshot(fs, k)?;
        delete::check_snapshot_needs_deletion(t, k, &mut nr_empty_interior);
        Ok(())
    })?;

    if nr_empty_interior != 0 {
        let mut msg = LogMsg::with_level(fs, LogLevel::NOTICE);

        let _ = writeln!(msg, "Found {nr_empty_interior} empty interior snapshot nodes");
        passes::run_explicit(fs, &mut msg, c::bch_recovery_pass::BCH_RECOVERY_PASS_delete_dead_interior_snapshots,
                             c::bch_run_recovery_pass_flags::empty())?;
    }
    Ok(())
}

crate::recovery_pass!(bch2_snapshots_read => snapshots_read);

// ── Lookups ──────────────────────────────────────────────────────────────

/// ENOENT if there's none.
pub fn lookup(t: &TransAttempt<'_, '_>, id: u32) -> Result<c::Snapshot, BchError> {
    t.bkey_get_val::<c::Snapshot>(c::btree_id::snapshots, pos(0, id as u64),
                                  BtreeIterFlags::empty())
}

/// ENOENT if there's none.
pub fn lookup_key(t: &TransAttempt<'_, '_>, id: u32) -> Result<BkeyI<c::Snapshot>, BchError> {
    t.bkey_get_key::<c::Snapshot>(c::btree_id::snapshots, pos(0, id as u64),
                                  BtreeIterFlags::empty())
}

/// Whether @k is in a snapshot that doesn't exist - reported, and @k deleted
/// or its snapshot reconstructed - for the caller to skip it.
pub fn check_key_has_snapshot(
    t:    &TransAttempt<'_, '_>,
    iter: &BtreeIter<'_>,
    k:    BkeySC<'_>,
) -> Result<bool, BchError> {
    if t.fs().snapshots.id_state(k.k.p.snapshot) == c::snapshot_id_state::SNAPSHOT_ID_live {
        return Ok(false);
    }
    // SAFETY: BtreeIter holds its iterator in an UnsafeCell, and nothing else
    // has it for the call
    let iter = unsafe { &mut *iter.raw() };
    check_snapshots::check_key_has_snapshot(t, Some(iter), k)
}

/// The snapshots that have overwritten the key at @pos in @btree: on each
/// branch, the one closest to @pos.snapshot.
pub fn overwrites(t: &TransAttempt<'_, '_>, btree: c::btree_id, pos: c::bpos)
    -> Result<KVVec<u32>, BchError>
{
    let mut ids = KVVec::new();

    if !t.fs().snapshots.has_children(pos.snapshot) {
        return Ok(ids);
    }

    // From the version before @pos's - in the snapshot with the next lower
    // ID; @pos.snapshot has children, so it isn't 0 - down
    let mut iter = BtreeIter::new(t, btree, spos(pos.inode, pos.offset, pos.snapshot - 1),
                                  BtreeIterFlags::ALL_SNAPSHOTS);
    iter.for_each_reverse_norestart(t, spos(pos.inode, pos.offset, 0), |_, k| {
        let id = k.k.p.snapshot;
        if is_ancestor(t, id, pos.snapshot) &&
           !ids.iter().any(|&i| is_ancestor(t, id, i)) {
            ids.push(id, GFP_KERNEL)?;
        }
        Ok(())
    })?;
    Ok(ids)
}

// ── The tree, for error messages ─────────────────────────────────────────

/// Node @id's line of tree_keys_to_text(): its state, its subvolume's path,
/// its data.
#[cold]
fn node_to_text(out: &mut Printbuf, t: &TransAttempt<'_, '_>, id: u32) -> Result<(), BchError> {
    let _ = write!(out, "{id} \t");

    match lookup(t, id) {
        Err(e) if is_restart(&e) => return Err(e),
        Err(e) => { let _ = write!(out, "{}", e.msg()); }
        Ok(s) => {
            let _ = write!(out, "{} ", c::bch_snapshot_state(s.state.get()));
            let subvol = s.subvol.get();
            if subvol != 0 {
                let _ = write!(out, "subvol {subvol}");
            }
            out.tab();

            if subvol != 0 {
                match subvolume::get(t, subvol, false) {
                    Err(e) if is_restart(&e) => return Err(e),
                    Err(e) => { let _ = write!(out, "{}", e.msg()); }
                    Ok(sv) => crate::namei::inum_to_path(
                        t, c::subvol_inum { subvol: subvol as u64, inum: sv.inode.get() }, out)?,
                }
            }
            out.tab();

            // external_sectors is counter 2, and only the extents btree
            // (btree 0, the default here) carries it - so this one read is
            // the snapshot's total on-disk usage.
            let mut v = [0u64; 3];
            crate::accounting::mem_read(t.fs(), &c::disk_accounting_pos::from_arm(
                    c::bch_acct_snapshot { id, btree: 0 }), &mut v);
            out.human_readable_u64(v[2] << 9);
            out.tab_rjust();
        }
    }
    out.newline();

    // Room for the next line now, while dropping locks to get it is cheap:
    // the next attempt relocks
    let _ = t.allocate_dropping_locks_norelock(|gfp| out.make_room_gfp(1024, gfp).then_some(()));
    Ok(())
}

/// The snapshot keys of the tree @start is in, indented by depth, for an
/// error message. Each node is its own attempt - so not for a caller in the
/// middle of one: the dump would restart it.
#[cold]
pub fn tree_keys_to_text(out: &mut Printbuf, trans: &BtreeTrans<'_>, start: u32)
    -> Result<(), BchError>
{
    let fs = trans.fs();
    let max_depth = fs.snapshots.with_table(|t| t.subtree(start).map(|(_, d)| d).max().unwrap_or(0));

    out.tabstops_reset();
    out.tabstop_push(out.indent_level() + 12 + 2 * max_depth as u32);
    out.tabstop_push(20);
    out.tabstop_push(40);
    out.tabstop_push(12);

    let (mut depth, mut prev_depth) = (0i32, 0i32);
    let mut id = start;
    while id != 0 && id <= start {
        let d = depth - prev_depth;
        if d > 0 {
            out.indent_add(d as u32 * 2);
        } else {
            out.indent_sub((-d) as u32 * 2);
        }
        prev_depth = depth;

        lockrestart_do(trans, |t| {
            let restore = out.state_save();
            node_to_text(out, t, id).inspect_err(|e| if is_restart(e) {
                out.state_restore(restore);
            })
        })?;

        id = fs.snapshots.with_table(|t| t.tree_next(id, &mut depth));
    }

    out.indent_sub(prev_depth as u32 * 2);
    Ok(())
}
