// SPDX-License-Identifier: GPL-2.0

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

#include "bcachefs.h"

#include "alloc/accounting.h"

#include "btree/update.h"
#include "btree/write_buffer.h"

#include "fs/namei.h"

#include "init/error.h"
#include "init/passes.h"
#include "init/recovery.h"

#include "snapshots/snapshot.h"

/*
 * Snapshot trees:
 *
 * Keys in BTREE_ID_snapshot_trees identify a whole tree of snapshot nodes; they
 * exist to provide a stable identifier for the whole lifetime of a snapshot
 * tree.
 */

__cold void bch2_snapshot_tree_to_text(struct printbuf *out, struct bch_fs *c,
				struct bkey_s_c k)
{
	struct bkey_s_c_snapshot_tree t = bkey_s_c_to_snapshot_tree(k);

	prt_printf(out, "subvol %u root snapshot %u",
		   le32_to_cpu(t.v->master_subvol),
		   le32_to_cpu(t.v->root_snapshot));
}

int bch2_snapshot_tree_validate(struct bch_fs *c, struct bkey_s_c k,
				const struct bkey_validate_context *from)
{
	int ret = 0;

	bkey_fsck_err_on(bkey_gt(k.k->p, POS(0, U32_MAX)) ||
			 bkey_lt(k.k->p, POS(0, 1)),
			 c, snapshot_tree_pos_bad,
			 "bad pos");
fsck_err:
	return ret;
}

int bch2_snapshot_tree_lookup(struct btree_trans *trans, u32 id,
			      struct bch_snapshot_tree *s)
{
	int ret = bch2_bkey_get_val_typed(trans, BTREE_ID_snapshot_trees, POS(0, id),
					  0, snapshot_tree, s);

	if (bch2_err_matches(ret, ENOENT))
		ret = bch_err_throw(trans->c, ENOENT_snapshot_tree);
	return ret;
}

struct bkey_i_snapshot_tree *
__bch2_snapshot_tree_create(struct btree_trans *trans)
{
	CLASS(btree_iter_uninit, iter)(trans);
	int ret = bch2_bkey_get_empty_slot(trans, &iter,
			BTREE_ID_snapshot_trees, POS_MIN, POS(0, U32_MAX));
	if (ret == -BCH_ERR_ENOSPC_btree_slot)
		ret = bch_err_throw(trans->c, ENOSPC_snapshot_tree);
	if (ret)
		return ERR_PTR(ret);

	return bch2_bkey_alloc(trans, &iter, 0, snapshot_tree);
}

/* Snapshot ancestor lookups: */

static bool __bch2_snapshot_is_ancestor_early(struct snapshot_table *t, u32 id, u32 ancestor)
{
	while (id && id < ancestor) {
		const struct snapshot_t *s = __snapshot_t(t, id);
		id = s ? s->parent : 0;
	}
	return id == ancestor;
}

bool bch2_snapshot_is_ancestor_early(struct bch_fs *c, u32 id, u32 ancestor)
{
	guard(rcu)();
	return __bch2_snapshot_is_ancestor_early(rcu_dereference(c->snapshots.table), id, ancestor);
}

static inline u32 get_ancestor_below(struct snapshot_table *t, u32 id, u32 ancestor)
{
	const struct snapshot_t *s = __snapshot_t(t, id);
	if (!s)
		return 0;

	if (s->skip[2] <= ancestor)
		return s->skip[2];
	if (s->skip[1] <= ancestor)
		return s->skip[1];
	if (s->skip[0] <= ancestor)
		return s->skip[0];
	return s->parent;
}

static bool test_ancestor_bitmap(struct snapshot_table *t, u32 id, u32 ancestor)
{
	const struct snapshot_t *s = __snapshot_t(t, id);
	if (!s)
		return false;

	return test_bit(ancestor - id - 1, s->is_ancestor);
}

static noinline __cold
void bch2_is_ancestor_trace_fastpath(struct printbuf *out,
				     struct snapshot_table *t,
				     u32 id, u32 ancestor)
{
	prt_printf(out, "  fastpath: %u", id);
	while (id && id < ancestor - IS_ANCESTOR_BITMAP) {
		u32 next = get_ancestor_below(t, id, ancestor);
		prt_printf(out, " -> %u", next);
		id = next;
	}
	if (id && id < ancestor)
		prt_printf(out, " bitmap[%u]=%u",
			   ancestor - id - 1,
			   test_ancestor_bitmap(t, id, ancestor));
	prt_newline(out);
}

static noinline __cold
void bch2_is_ancestor_trace_slowpath(struct printbuf *out,
				     struct snapshot_table *t,
				     u32 id, u32 ancestor)
{
	prt_printf(out, "  slowpath: %u", id);
	while (id && id < ancestor) {
		const struct snapshot_t *s = __snapshot_t(t, id);
		u32 next = s ? s->parent : 0;
		prt_printf(out, " -> %u", next);
		id = next;
	}
	prt_newline(out);
}

static noinline __cold
void bch2_is_ancestor_trace_btree(struct printbuf *out,
				  struct btree_trans *trans,
				  u32 id, u32 ancestor)
{
	prt_printf(out, "  btree:    %u", id);
	while (id && id < ancestor) {
		struct bch_snapshot s;
		int ret = bch2_snapshot_lookup(trans, id, &s);
		if (ret) {
			prt_printf(out, " (lookup error %i)", ret);
			break;
		}
		u32 next = le32_to_cpu(s.parent);
		prt_printf(out, " -> %u", next);
		id = next;
	}
	prt_newline(out);
}

static noinline __cold
void bch2_snapshot_is_ancestor_debug(struct btree_trans *trans,
				     u32 id, u32 ancestor,
				     bool fastpath_ret)
{
	struct bch_fs *c = trans->c;
	bool slowpath_ret;

	scoped_guard(rcu) {
		struct snapshot_table *t = rcu_dereference(c->snapshots.table);
		slowpath_ret = __bch2_snapshot_is_ancestor_early(t, id, ancestor);
	}

	if (fastpath_ret == slowpath_ret)
		return;

	CLASS(printbuf, buf)();
	prt_printf(&buf, "is_ancestor(%u, %u): fastpath=%u slowpath=%u\n",
		   id, ancestor, fastpath_ret, slowpath_ret);

	scoped_guard(rcu) {
		struct snapshot_table *t = rcu_dereference(c->snapshots.table);
		bch2_is_ancestor_trace_fastpath(&buf, t, id, ancestor);
		bch2_is_ancestor_trace_slowpath(&buf, t, id, ancestor);
	}

	bch2_is_ancestor_trace_btree(&buf, trans, id, ancestor);
	panic("%s", buf.buf);
}

bool __bch2_snapshot_is_ancestor(struct btree_trans *trans, u32 id, u32 ancestor)
{
	struct bch_fs *c = trans->c;
	u32 orig_id = id;
	bool ret;

	scoped_guard(rcu) {
		struct snapshot_table *t = rcu_dereference(c->snapshots.table);

		if (unlikely(recovery_pass_will_run(c, BCH_RECOVERY_PASS_check_snapshots)))
			return __bch2_snapshot_is_ancestor_early(t, id, ancestor);

		if (likely(ancestor >= IS_ANCESTOR_BITMAP))
			while (id && id < ancestor - IS_ANCESTOR_BITMAP)
				id = get_ancestor_below(t, id, ancestor);

		ret = id && id < ancestor
			? test_ancestor_bitmap(t, id, ancestor)
			: id == ancestor;
	}

	if (IS_ENABLED(CONFIG_BCACHEFS_DEBUG))
		bch2_snapshot_is_ancestor_debug(trans, orig_id, ancestor, ret);

	return ret;
}

/* In-memory snapshot table: */

static noinline struct snapshot_t *__snapshot_t_mut(struct bch_fs *c, u32 id)
{
	size_t idx = U32_MAX - id;
	struct snapshot_table *new, *old;

	size_t new_bytes = roundup_pow_of_two(struct_size(new, s, idx + 1));
	size_t new_size = (new_bytes - sizeof(*new)) / sizeof(new->s[0]);

	if (unlikely(new_bytes > INT_MAX))
		return NULL;

	new = kvzalloc(new_bytes, GFP_KERNEL);
	if (!new)
		return NULL;

	new->nr = new_size;

	old = rcu_dereference_protected(c->snapshots.table, true);
	if (old)
		memcpy(new->s, old->s, sizeof(old->s[0]) * old->nr);

	rcu_assign_pointer(c->snapshots.table, new);
	kvfree_rcu(old, rcu);

	return &rcu_dereference_protected(c->snapshots.table,
				lockdep_is_held(&c->snapshots.table_lock))->s[idx];
}

struct snapshot_t *bch2_snapshot_t_mut(struct bch_fs *c, u32 id)
{
	size_t idx = U32_MAX - id;
	struct snapshot_table *table =
		rcu_dereference_protected(c->snapshots.table,
				lockdep_is_held(&c->snapshots.table_lock));

	if (likely(table && idx < table->nr))
		return &table->s[idx];

	return __snapshot_t_mut(c, id);
}

/* Snapshot node state */

const char *bch2_snapshot_state_str(enum bch_snapshot_state s)
{
	switch (s) {
#define x(n, v) case SNAPSHOT_STATE_##n: return #n;
	BCH_SNAPSHOT_STATES()
#undef x
		default: return "(invalid state)";
	}
}

/* SUBVOL_OBSOLETE is derived from s->subvol: assign that field before calling */
void bch2_snapshot_state_set(struct bch_snapshot *s, enum bch_snapshot_state n)
{
	switch (n) {
	case SNAPSHOT_STATE_live:
		SET_BCH_SNAPSHOT_SUBVOL_OBSOLETE(s, s->subvol != 0);
		SET_BCH_SNAPSHOT_WILL_DELETE_OBSOLETE(s, 0);
		SET_BCH_SNAPSHOT_DELETED_OBSOLETE(s, 0);
		SET_BCH_SNAPSHOT_NO_KEYS_OBSOLETE(s, 0);
		break;
	case SNAPSHOT_STATE_will_delete:
		SET_BCH_SNAPSHOT_SUBVOL_OBSOLETE(s, 0);
		SET_BCH_SNAPSHOT_WILL_DELETE_OBSOLETE(s, 1);
		SET_BCH_SNAPSHOT_DELETED_OBSOLETE(s, 0);
		SET_BCH_SNAPSHOT_NO_KEYS_OBSOLETE(s, 0);
		break;
	case SNAPSHOT_STATE_no_keys:
		SET_BCH_SNAPSHOT_SUBVOL_OBSOLETE(s, 0);
		SET_BCH_SNAPSHOT_WILL_DELETE_OBSOLETE(s, 0);
		SET_BCH_SNAPSHOT_DELETED_OBSOLETE(s, 0);
		SET_BCH_SNAPSHOT_NO_KEYS_OBSOLETE(s, 1);
		break;
	case SNAPSHOT_STATE_deleted:
		SET_BCH_SNAPSHOT_SUBVOL_OBSOLETE(s, 0);
		SET_BCH_SNAPSHOT_WILL_DELETE_OBSOLETE(s, 0);
		SET_BCH_SNAPSHOT_DELETED_OBSOLETE(s, 1);
		SET_BCH_SNAPSHOT_NO_KEYS_OBSOLETE(s, 0);
		break;
	}

	s->state = cpu_to_le32(n);
}

/* Snapshot btree key to_text/validate: */

__cold void bch2_snapshot_to_text(struct printbuf *out, const struct bch_snapshot *s)
{
	prt_printf(out, "parent %10u children %10u %10u subvol %u tree %u",
	       le32_to_cpu(s->parent),
	       le32_to_cpu(s->children[0]),
	       le32_to_cpu(s->children[1]),
	       le32_to_cpu(s->subvol),
	       le32_to_cpu(s->tree));

	prt_printf(out, " depth %u skiplist %u %u %u",
		   le32_to_cpu(s->depth),
		   le32_to_cpu(s->skip[0]),
		   le32_to_cpu(s->skip[1]),
		   le32_to_cpu(s->skip[2]));

	static const char * const obsolete_flag_strs[] = {
		"will_delete", "subvol", "deleted", "no_keys", NULL
	};
	u32 state = le32_to_cpu(s->state);
	u32 flags = le32_to_cpu(s->flags);

	if (!state) {
		/* Not upgraded: no state field, the obsolete flags are the truth */
		prt_printf(out, " %s (not upgraded, flags 0x%x:",
			   bch2_snapshot_state_str(bch2_snapshot_state_from_flags(s)),
			   flags);
		prt_bitflags(out, obsolete_flag_strs, flags);
		prt_printf(out, ")");
	} else if (!bch2_snapshot_state_valid(state)) {
		prt_printf(out, " state 0x%x invalid (flags 0x%x:", state, flags);
		prt_bitflags(out, obsolete_flag_strs, flags);
		prt_printf(out, ")");
	} else {
		prt_printf(out, " %s", bch2_snapshot_state_str(state));

		/*
		 * The obsolete flags are dual-written shadows of the state field
		 * and subvol pointer (bch2_snapshot_state_set()); recompute what
		 * the dual-write would produce and print only divergence:
		 */
		struct bch_snapshot expect = *s;
		bch2_snapshot_state_set(&expect, state);

		if (BCH_SNAPSHOT_WILL_DELETE_OBSOLETE(s) != BCH_SNAPSHOT_WILL_DELETE_OBSOLETE(&expect))
			prt_printf(out, " will_delete_obsolete=%llu", BCH_SNAPSHOT_WILL_DELETE_OBSOLETE(s));
		if (BCH_SNAPSHOT_SUBVOL_OBSOLETE(s) != BCH_SNAPSHOT_SUBVOL_OBSOLETE(&expect))
			prt_printf(out, " subvol_obsolete=%llu", BCH_SNAPSHOT_SUBVOL_OBSOLETE(s));
		if (BCH_SNAPSHOT_DELETED_OBSOLETE(s) != BCH_SNAPSHOT_DELETED_OBSOLETE(&expect))
			prt_printf(out, " deleted_obsolete=%llu", BCH_SNAPSHOT_DELETED_OBSOLETE(s));
		if (BCH_SNAPSHOT_NO_KEYS_OBSOLETE(s) != BCH_SNAPSHOT_NO_KEYS_OBSOLETE(&expect))
			prt_printf(out, " no_keys_obsolete=%llu", BCH_SNAPSHOT_NO_KEYS_OBSOLETE(s));
	}
}

__cold void bch2_snapshot_key_to_text(struct printbuf *out, struct bch_fs *c,
			       struct bkey_s_c k)
{
	struct bch_snapshot snapshot;
	bkey_val_copy_pad(&snapshot, bkey_s_c_to_snapshot(k));
	bch2_snapshot_to_text(out, &snapshot);
}

int bch2_snapshot_validate(struct bch_fs *c, struct bkey_s_c k,
			   const struct bkey_validate_context *from)
{
	struct bkey_s_c_snapshot s;
	u32 i, id;
	int ret = 0;

	bkey_fsck_err_on(bkey_gt(k.k->p, POS(0, U32_MAX)) ||
			 bkey_lt(k.k->p, POS(0, 1)),
			 c, snapshot_pos_bad,
			 "bad pos");

	s = bkey_s_c_to_snapshot(k);

	id = le32_to_cpu(s.v->parent);
	bkey_fsck_err_on(id && id <= k.k->p.offset,
			 c, snapshot_parent_bad,
			 "bad parent node (%u <= %llu)",
			 id, k.k->p.offset);

	bkey_fsck_err_on(le32_to_cpu(s.v->children[0]) < le32_to_cpu(s.v->children[1]),
			 c, snapshot_children_not_normalized,
			 "children not normalized");

	bkey_fsck_err_on(s.v->children[0] && s.v->children[0] == s.v->children[1],
			 c, snapshot_child_duplicate,
			 "duplicate child nodes");

	for (i = 0; i < 2; i++) {
		id = le32_to_cpu(s.v->children[i]);

		bkey_fsck_err_on(id >= k.k->p.offset,
				 c, snapshot_child_bad,
				 "bad child node (%u >= %llu)",
				 id, k.k->p.offset);
	}

	if (bkey_has_field(k.k, snapshot, skip)) {
		bkey_fsck_err_on(le32_to_cpu(s.v->skip[0]) > le32_to_cpu(s.v->skip[1]) ||
				 le32_to_cpu(s.v->skip[1]) > le32_to_cpu(s.v->skip[2]),
				 c, snapshot_skiplist_not_normalized,
				 "skiplist not normalized");

		for (i = 0; i < ARRAY_SIZE(s.v->skip); i++) {
			id = le32_to_cpu(s.v->skip[i]);

			bkey_fsck_err_on(id && id < le32_to_cpu(s.v->parent),
					 c, snapshot_skiplist_bad,
					 "bad skiplist node %u", id);
		}
	}

	if (bkey_has_field(k.k, snapshot, pad))
		bkey_fsck_err_on(s.v->pad,
				 c, snapshot_pad_nonzero,
				 "reserved pad field nonzero");

	/*
	 * Commit-only checks - defense in depth: catch the buggy writer in
	 * the act, with a backtrace. Never applied to existing keys (the
	 * invalid-bkey machinery's remedy is dropping the key; on-disk
	 * violations are fsck's to handle, gently). Zero state is legal -
	 * pre-upgrade keys are rewritten with only their other fields
	 * touched:
	 */
	if (from->from == BKEY_VALIDATE_commit && !c->opts.no_commit_validate) {
		u32 state = bkey_has_field(k.k, snapshot, state)
			? bch2_snapshot_state(s.v)
			: 0;

		bkey_fsck_err_on(state && !bch2_snapshot_state_valid(state),
				 c, snapshot_state_bad,
				 "invalid state 0x%x", state);

		/*
		 * A subvol backref is only legal on a live or will_delete leaf:
		 * subvolumes only reference leaves, and no_keys/deleted nodes
		 * were interior when they were emptied - a dead leaf is deleted
		 * outright; the no_keys parking state exists only because an
		 * interior node can't be removed from the tree at runtime.
		 * will_delete retains the backref: it points at the subvolume's
		 * tombstone, which the deletion path checks
		 * (check_should_delete_leaf()).
		 */
		bkey_fsck_err_on(s.v->subvol &&
				 (s.v->children[0] ||
				  (state &&
				   state != SNAPSHOT_STATE_live &&
				   state != SNAPSHOT_STATE_will_delete)),
				 c, snapshot_should_not_have_subvol,
				 "snapshot with subvol must be a live or will_delete leaf");
	}
fsck_err:
	return ret;
}

/* Snapshot btree triggers: */

static int bch2_mark_snapshot(struct btree_trans *trans, struct bkey_s_c new)
{
	struct bch_fs *c = trans->c;
	u32 id = new.k->p.offset;

	guard(mutex)(&c->snapshots.table_lock);

	struct snapshot_t *t = bch2_snapshot_t_mut(c, id);
	if (!t)
		return bch_err_throw(c, ENOMEM_mark_snapshot);

	if (new.k->type == KEY_TYPE_snapshot) {
		struct bch_snapshot s;
		bkey_val_copy_pad(&s, bkey_s_c_to_snapshot(new));
		enum bch_snapshot_state state = bch2_snapshot_state_compat(&s);

		t->state	= (state != SNAPSHOT_STATE_no_keys &&
				   state != SNAPSHOT_STATE_deleted)
			? SNAPSHOT_ID_live
			: SNAPSHOT_ID_deleted;
		t->parent	= le32_to_cpu(s.parent);
		t->children[0]	= le32_to_cpu(s.children[0]);
		t->children[1]	= le32_to_cpu(s.children[1]);
		t->subvol	= le32_to_cpu(s.subvol);
		t->tree		= le32_to_cpu(s.tree);
		t->depth	= le32_to_cpu(s.depth);
		t->skip[0]	= le32_to_cpu(s.skip[0]);
		t->skip[1]	= le32_to_cpu(s.skip[1]);
		t->skip[2]	= le32_to_cpu(s.skip[2]);

		unsigned long is_ancestor[BITS_TO_LONGS(IS_ANCESTOR_BITMAP)] = {};
		u32 parent = id;

		while ((parent = bch2_snapshot_parent_early(c, parent)) &&
		       parent - id - 1 < IS_ANCESTOR_BITMAP)
			__set_bit(parent - id - 1, is_ancestor);

		/*
		 * Readers access is_ancestor under RCU without locks.
		 * memcpy is sufficient here because readers can tolerate
		 * seeing a mix of old and new values - they'll just take
		 * a slower path. barrier_data prevents the compiler from
		 * eliding the temporary and writing directly to t->is_ancestor.
		 */
		barrier_data(is_ancestor);
		memcpy(t->is_ancestor, is_ancestor, sizeof(t->is_ancestor));

		if (state == SNAPSHOT_STATE_will_delete) {
			/*
			 * Schedule the deleter. bch2_mark_snapshot may run as a
			 * BTREE_TRIGGER_atomic trigger - btree write locks held,
			 * committed to the commit - so the schedule must be
			 * ephemeral (no sb_lock) and best-effort: we ignore the
			 * return, since an error here would take the filesystem
			 * emergency read-only.
			 *
			 * In recovery this injects delete_dead_snapshots into the
			 * running passes so it runs (in listing order) before the
			 * content checks; otherwise it schedules the async runner.
			 */
			CLASS(printbuf, buf)();
			bch2_run_explicit_recovery_pass(c, &buf,
					BCH_RECOVERY_PASS_delete_dead_snapshots,
					RUN_RECOVERY_PASS_ephemeral);
		}
	} else {
		memset(t, 0, sizeof(*t));
	}

	return 0;
}

int bch2_snapshot_trigger(struct btree_trans *trans, struct btree_trigger_op op)
{
	if (op.flags & BTREE_TRIGGER_transactional)
		bch2_clear_btree_clean(trans->c, BTREE_ID_snapshots);

	if (op.flags & BTREE_TRIGGER_atomic)
		try(bch2_mark_snapshot(trans, op.new.s_c));

	return 0;
}

/* Snapshot tree traversal: */

static u32 bch2_snapshot_child(struct snapshot_table *t,
			       u32 id, unsigned child)
{
	return __snapshot_t(t, id)->children[child];
}

static u32 bch2_snapshot_left_child(struct snapshot_table *t, u32 id)
{
	return bch2_snapshot_child(t, id, 0);
}

static u32 bch2_snapshot_right_child(struct snapshot_table *t, u32 id)
{
	return bch2_snapshot_child(t, id, 1);
}

u32 __bch2_snapshot_tree_next(struct bch_fs *c, struct snapshot_table *t, u32 id, unsigned *depth)
{
	int _depth;
	if (!depth)
		depth = &_depth;

	u32 n = bch2_snapshot_left_child(t, id);
	if (n) {
		(*depth)++;
		return n;
	}

	u32 parent;
	while ((parent = __bch2_snapshot_parent(c, t, id))) {
		(*depth)--;
		n = bch2_snapshot_right_child(t, parent);
		if (n && n != id) {
			(*depth)++;
			return n;
		}
		id = parent;
	}

	return 0;
}

u32 bch2_snapshot_tree_next(struct bch_fs *c, u32 id, unsigned *depth)
{
	guard(rcu)();
	return __bch2_snapshot_tree_next(c, rcu_dereference(c->snapshots.table), id, depth);
}

/* Snapshot btree lookups: */

int bch2_snapshot_lookup(struct btree_trans *trans, u32 id,
			 struct bch_snapshot *s)
{
	return bch2_bkey_get_val_typed(trans, BTREE_ID_snapshots, POS(0, id), 0, snapshot, s);
}

/*
 * As bch2_snapshot_lookup(), but keeps the key: the topology checks report on
 * nodes they didn't start from, and naming one by id alone leaves a field
 * report with no way to see what was wrong with it.
 */
int bch2_snapshot_lookup_key(struct btree_trans *trans, u32 id,
			     struct bkey_i_snapshot *k)
{
	return bch2_bkey_get_i_typed(trans, BTREE_ID_snapshots, POS(0, id), 0, snapshot, k);
}

/* Key snapshot overwrite checks: */

int __bch2_get_snapshot_overwrites(struct btree_trans *trans,
				   enum btree_id btree, struct bpos pos,
				   snapshot_id_list *s)
{
	struct bch_fs *c = trans->c;
	struct bkey_s_c k;
	int ret = 0;

	for_each_btree_key_reverse_norestart(trans, iter, btree, bpos_predecessor(pos),
					     BTREE_ITER_all_snapshots, k, ret) {
		if (!bkey_eq(k.k->p, pos))
			break;

		if (!bch2_snapshot_is_ancestor(trans, k.k->p.snapshot, pos.snapshot) ||
		    snapshot_list_has_ancestor(trans, s, k.k->p.snapshot))
			continue;

		try(snapshot_list_add(c, s, k.k->p.snapshot));
	}

	return ret;
}

int __bch2_key_has_snapshot_overwrites(struct btree_trans *trans,
				       enum btree_id id,
				       struct bpos pos)
{
	struct bkey_s_c k;
	int ret;

	for_each_btree_key_reverse_norestart(trans, iter, id, bpos_predecessor(pos),
					     BTREE_ITER_not_extents|
					     BTREE_ITER_all_snapshots,
					     k, ret) {
		if (!bkey_eq(pos, k.k->p))
			break;

		if (bch2_snapshot_is_ancestor(trans, k.k->p.snapshot, pos.snapshot))
			return 1;
	}

	return ret;
}

/* Snapshot node creation: */

static int create_snapids(struct btree_trans *trans, u32 parent, u32 tree,
			  u32 *new_snapids,
			  u32 *snapshot_subvols,
			  unsigned nr_snapids)
{
	struct bch_fs *c = trans->c;
	u32 depth = bch2_snapshot_depth(c, parent);

	CLASS(btree_iter, iter)(trans, BTREE_ID_snapshots, POS_MIN, BTREE_ITER_intent);
	struct bkey_s_c k = bkey_try(bch2_btree_iter_peek(&iter));

	for (unsigned i = 0; i < nr_snapids; i++) {
		k = bkey_try(bch2_btree_iter_prev_slot(&iter));

		if (!k.k || !k.k->p.offset) {
			return bch_err_throw(c, ENOSPC_snapshot_create);
		}

		struct bkey_i_snapshot *n = errptr_try(bch2_bkey_alloc(trans, &iter, 0, snapshot));

		n->v.flags	= 0;
		n->v.parent	= cpu_to_le32(parent);
		n->v.subvol	= cpu_to_le32(snapshot_subvols[i]);
		n->v.tree	= cpu_to_le32(tree);
		n->v.depth	= cpu_to_le32(depth);
		n->v.btime.lo	= cpu_to_le64(bch2_current_time(c));
		n->v.btime.hi	= 0;

		for (unsigned j = 0; j < ARRAY_SIZE(n->v.skip); j++)
			n->v.skip[j] = cpu_to_le32(bch2_snapshot_skiplist_get(c, parent));

		bubble_sort(n->v.skip, ARRAY_SIZE(n->v.skip), cmp_le32);

		bch2_snapshot_state_set(&n->v, SNAPSHOT_STATE_live);

		try(bch2_mark_snapshot(trans, bkey_i_to_s_c(&n->k_i)));

		new_snapids[i]	= iter.pos.offset;
	}

	return 0;
}

/*
 * Create new snapshot IDs as children of an existing snapshot ID:
 */
static int bch2_snapshot_node_create_children(struct btree_trans *trans, u32 parent,
			      u32 *new_snapids,
			      u32 *snapshot_subvols,
			      unsigned nr_snapids)
{
	struct bkey_i_snapshot *n_parent =
		bch2_bkey_get_mut_typed(trans, BTREE_ID_snapshots, POS(0, parent), 0, snapshot);
	int ret = PTR_ERR_OR_ZERO(n_parent);
	if (unlikely(ret)) {
		if (bch2_err_matches(ret, ENOENT))
			bch_err(trans->c, "snapshot %u not found", parent);
		return ret;
	}

	if (n_parent->v.children[0] || n_parent->v.children[1]) {
		bch_err(trans->c, "Trying to add child snapshot nodes to parent that already has children");
		return bch_err_throw(trans->c, EINVAL_snapshot_parent_already_has_children);
	}

	ret = create_snapids(trans, parent, le32_to_cpu(n_parent->v.tree),
			     new_snapids, snapshot_subvols, nr_snapids);
	if (ret)
		return ret;

	n_parent->v.children[0] = cpu_to_le32(new_snapids[0]);
	n_parent->v.children[1] = cpu_to_le32(new_snapids[1]);
	n_parent->v.subvol = 0;
	bch2_snapshot_state_set(&n_parent->v, SNAPSHOT_STATE_live);
	return 0;
}

/*
 * Create a snapshot node that is the root of a new tree:
 */
static int bch2_snapshot_node_create_tree(struct btree_trans *trans,
			      u32 *new_snapids,
			      u32 *snapshot_subvols,
			      unsigned nr_snapids)
{
	struct bkey_i_snapshot_tree *n_tree =
		errptr_try(__bch2_snapshot_tree_create(trans));

	try(create_snapids(trans, 0, n_tree->k.p.offset,
			   new_snapids, snapshot_subvols, nr_snapids));

	n_tree->v.master_subvol	= cpu_to_le32(snapshot_subvols[0]);
	n_tree->v.root_snapshot	= cpu_to_le32(new_snapids[0]);
	return 0;
}

int bch2_snapshot_node_create(struct btree_trans *trans, u32 parent,
			      u32 *new_snapids,
			      u32 *snapshot_subvols,
			      unsigned nr_snapids)
{
	BUG_ON((parent == 0) != (nr_snapids == 1));
	BUG_ON((parent != 0) != (nr_snapids == 2));

	return parent
		? bch2_snapshot_node_create_children(trans, parent,
				new_snapids, snapshot_subvols, nr_snapids)
		: bch2_snapshot_node_create_tree(trans,
				new_snapids, snapshot_subvols, nr_snapids);

}

/* Module init/exit: */

/*
 * A topology repair rewrote a parent pointer: mark_snapshot recomputed that
 * node's table entry, but descendants' is_ancestor bitmaps were built under
 * the old topology. Re-mark everything (reverse: ancestors first). Ancestor
 * queries during check_snapshots itself use the parent-walking _early
 * variants, so the stale window ends before anything trusts the bitmaps.
 */
int bch2_snapshot_table_rebuild(struct btree_trans *trans)
{
	try(for_each_btree_key_reverse(trans, iter, BTREE_ID_snapshots, POS_MAX, 0, k,
		bch2_mark_snapshot(trans, k)));

	trans->c->snapshots.need_table_rebuild = false;
	return 0;
}

int bch2_snapshots_read(struct bch_fs *c)
{
	/*
	 * It's important that we check if we need to reconstruct snapshots
	 * before going RW, so we mark that pass as required in the superblock -
	 * otherwise, we could end up deleting keys with missing snapshot nodes
	 * instead
	 */
	BUG_ON(!test_bit(BCH_FS_new_fs, &c->flags) &&
	       test_bit(BCH_FS_may_go_rw, &c->flags));

	/*
	 * Initializing the is_ancestor bitmaps requires ancestors to already be
	 * initialized - so mark in reverse:
	 */
	CLASS(btree_trans, trans)(c);
	u32 nr_empty_interior = 0;
	try(for_each_btree_key_reverse(trans, iter, BTREE_ID_snapshots, POS_MAX, 0, k,
		bch2_mark_snapshot(trans, k) ?:
		bch2_check_snapshot_needs_deletion(trans, k, &nr_empty_interior)));

	if (nr_empty_interior) {
		CLASS(bch_log_msg_level, msg)(c, LOGLEVEL_notice);

		prt_printf(&msg.m, "Found %u empty interior snapshot nodes\n", nr_empty_interior);
		try(bch2_run_explicit_recovery_pass(c, &msg.m,
				BCH_RECOVERY_PASS_delete_dead_interior_snapshots, 0));
	}

	return 0;
}

void bch2_fs_snapshots_exit(struct bch_fs *c)
{
	percpu_free_rwsem(&c->snapshots.create_lock);
	kvfree(rcu_dereference_protected(c->snapshots.table, true));
}

void bch2_fs_snapshots_init_early(struct bch_fs *c)
{
	mutex_init(&c->snapshots.table_lock);

	mutex_init(&c->snapshots.delete.progress_lock);

	mutex_init(&c->snapshots.unlinked_lock);
}

int bch2_fs_snapshots_init(struct bch_fs *c)
{
	return percpu_init_rwsem(&c->snapshots.create_lock);
}

/* to_text() methods: */

static int snapshot_get_print(struct printbuf *out, struct btree_trans *trans, u32 id)
{
	prt_printf(out, "%u \t", id);

	struct bch_snapshot s;
	int ret = bch2_snapshot_lookup(trans, id, &s);
	if (bch2_err_matches(ret, BCH_ERR_transaction_restart))
		return ret;

	if (ret) {
		prt_str(out, bch2_err_str(ret));
	} else {
		prt_str(out, bch2_snapshot_state_str(bch2_snapshot_state(&s)));
		prt_char(out, ' ');
		if (s.subvol)
			prt_printf(out, "subvol %u", le32_to_cpu(s.subvol));

		prt_tab(out);

		if (s.subvol) {
			struct bch_subvolume subvol;
			ret = bch2_subvolume_get(trans, le32_to_cpu(s.subvol), false, &subvol);
			if (bch2_err_matches(ret, BCH_ERR_transaction_restart))
				return ret;

			if (ret)
				prt_str(out, bch2_err_str(ret));
			else
				try(bch2_inum_to_path(trans, (subvol_inum)
					{ le32_to_cpu(s.subvol), le64_to_cpu(subvol.inode) }, out));
		}

		prt_tab(out);

		/*
		 * external_sectors is counter 2, and extents is the only
		 * snapshot btree that carries it - so this one read is the
		 * snapshot's total on-disk usage.
		 */
		u64 v[3] = { 0 };
		try(bch2_snapshot_accounting_read(trans, id, BTREE_ID_extents, v));

		prt_human_readable_u64(out, v[2] << 9);
		prt_tab_rjust(out);
	}

	prt_newline(out);

	bool lock_dropped = false;
	allocate_dropping_locks_norelock(trans, lock_dropped,
			!bch2_printbuf_make_room_gfp(out, 1024, _gfp));
	return 0;
}

static unsigned snapshot_tree_max_depth(struct bch_fs *c, u32 start)
{
	unsigned depth = 0, max_depth = 0;

	guard(rcu)();
	struct snapshot_table *t = rcu_dereference(c->snapshots.table);

	__for_each_snapshot_child(c, t, start, &depth, id)
		max_depth = max(depth, max_depth);
	return max_depth;
}

__cold int bch2_snapshot_tree_keys_to_text(struct printbuf *out, struct btree_trans *trans, u32 start)
{
	printbuf_tabstops_reset(out);
	printbuf_tabstop_push(out, out->indent + 12 + 2 * snapshot_tree_max_depth(trans->c, start));
	printbuf_tabstop_push(out, 20);
	printbuf_tabstop_push(out, 40);
	printbuf_tabstop_push(out, 12);

	unsigned depth = 0, prev_depth = 0;
	for_each_snapshot_child(trans->c, start, &depth, id) {
		int d = depth - prev_depth;
		if (d > 0)
			printbuf_indent_add(out, d * 2);
		else
			printbuf_indent_sub(out, -d * 2);
		prev_depth = depth;

		try(lockrestart_do(trans, ({
			struct printbuf_restore restore = printbuf_state_save(out);
			int ret = snapshot_get_print(out, trans, id);
			if (bch2_err_matches(ret, BCH_ERR_transaction_restart))
				printbuf_state_restore(out, restore);
			ret;
		})));
	}

	printbuf_indent_sub(out, prev_depth * 2);

	return 0;
}

__cold void bch2_snapshot_id_list_to_text(struct printbuf *out, snapshot_id_list *s)
{
	bool first = true;
	darray_for_each(*s, i) {
		if (!first)
			prt_char(out, ' ');
		first = false;
		prt_printf(out, "%u", *i);
	}
}

