// SPDX-License-Identifier: GPL-2.0

/*
 * The damage btree: persistent record of which inodes have been damaged
 * by errors and repairs. See damage_format.h for the value format and
 * damage.h for the design notes.
 */

#include "bcachefs.h"

#include "btree/iter.h"
#include "btree/update.h"
#include "fs/inode.h"
#include "init/damage.h"
#include "init/error.h"
#include "sb/errors.h"
#include "snapshots/snapshot.h"
#include "snapshots/subvolume.h"

int bch2_damage_validate(struct bch_fs *c, struct bkey_s_c k,
			 const struct bkey_validate_context *from)
{
	struct bkey_s_c_damage d = bkey_s_c_to_damage(k);
	unsigned nr = bkey_val_bytes(k.k) / sizeof(d.v->entries[0]);
	unsigned prev = 0;
	int ret = 0;

	bkey_fsck_err_on(bkey_val_bytes(k.k) % sizeof(d.v->entries[0]),
			 c, damage_entries_bad,
			 "val size %zu not a multiple of entry size",
			 bkey_val_bytes(k.k));

	for (unsigned i = 0; i < nr; i++) {
		const bch_sb_field_error_entry_v2 *e = &d.v->entries[i];
		unsigned id = BCH_SB_ERROR_ENTRY_V2_ID(e);

		/* Strictly ascending ids: covers unsorted, dup and zero: */
		bkey_fsck_err_on(id <= prev,
				 c, damage_entries_bad,
				 "entry %u: id %u nr %llu (prev id %u)",
				 i, id, BCH_SB_ERROR_ENTRY_V2_NR(e), prev);
		prev = id;

		bkey_fsck_err_on(BCH_SB_ERROR_ENTRY_V2_FIRST(e) >
				 BCH_SB_ERROR_ENTRY_V2_LAST(e),
				 c, damage_entries_bad,
				 "entry %u: first occurrence after last", i);
	}
fsck_err:
	return ret;
}

void bch2_damage_to_text(struct printbuf *out, struct bch_fs *c, struct bkey_s_c k)
{
	struct bkey_s_c_damage d = bkey_s_c_to_damage(k);
	unsigned nr = bkey_val_bytes(k.k) / sizeof(d.v->entries[0]);

	for (unsigned i = 0; i < nr; i++) {
		const bch_sb_field_error_entry_v2 *e = &d.v->entries[i];

		if (i)
			prt_newline(out);
		bch2_sb_error_id_to_text(out, BCH_SB_ERROR_ENTRY_V2_ID(e));
		prt_str(out, " nr ");
		bch2_prt_error_nr(out, BCH_SB_ERROR_ENTRY_V2_NR(e));
		prt_str(out, " first ");
		bch2_prt_datetime(out, BCH_SB_ERROR_ENTRY_V2_FIRST(e));
		prt_str(out, " last ");
		bch2_prt_datetime(out, BCH_SB_ERROR_ENTRY_V2_LAST(e));
	}
}

/*
 * Record that @err damaged the inode at @pos, in the caller's transaction
 * - the record commits with the repair that does the damage, so a crash
 * can't separate the two, and a discarded restart discards the increment
 * along with the repair it belonged to. Both key-position conventions are
 * accepted: a zero inode field means the inum is in the offset field
 * (inode keys live at (0, inum, snapshot)).
 *
 * Also feeds the in-memory damaged-paths report (the end-of-fsck summary
 * and the fsck_damaged_paths debugfs file), which stays the reporting
 * surface until reporting reads the damage btree. In -n mode the btree
 * update never commits while the in-memory report still populates -
 * exactly right.
 */
int bch2_damage_record(struct btree_trans *trans, struct bpos pos,
		       enum bch_sb_error_id err)
{
	u64 inum = pos.inode ?: pos.offset;

	bch2_fsck_damaged(trans, pos, err);

	CLASS(btree_iter, iter)(trans, BTREE_ID_damage,
				SPOS(0, inum, pos.snapshot),
				BTREE_ITER_all_snapshots|BTREE_ITER_intent);
	struct bkey_s_c k = bkey_try(bch2_btree_iter_peek_slot(&iter));

	const bch_sb_field_error_entry_v2 *old = NULL;
	unsigned nr = 0, idx = 0;
	bool found = false;

	if (k.k->type == KEY_TYPE_damage) {
		struct bkey_s_c_damage d = bkey_s_c_to_damage(k);

		old = d.v->entries;
		nr = bkey_val_bytes(k.k) / sizeof(d.v->entries[0]);

		while (idx < nr && BCH_SB_ERROR_ENTRY_V2_ID(&old[idx]) < err)
			idx++;
		found = idx < nr && BCH_SB_ERROR_ENTRY_V2_ID(&old[idx]) == err;
	}

	unsigned bytes = (nr + !found) * sizeof(*old);

	/*
	 * bkey.u64s is a u8 - growing the value past the max would wrap it,
	 * corrupting the key, in the same transaction as a repair. Skip
	 * recording new error ids once full; the in-memory report above
	 * still surfaces them:
	 */
	if (!found && bytes > BKEY_VAL_U64s_MAX * sizeof(u64))
		return 0;

	struct bkey_i_damage *n =
		errptr_try(bch2_trans_kmalloc(trans, sizeof(*n) + bytes));

	bkey_damage_init(&n->k_i);
	n->k.p = iter.pos;
	set_bkey_val_bytes(&n->k, bytes);
	if (nr)
		memcpy(n->v.entries, old, nr * sizeof(*old));

	u64 now = ktime_get_real_seconds();

	bch_sb_field_error_entry_v2 *e = &n->v.entries[idx];
	if (!found) {
		memmove(e + 1, e, (nr - idx) * sizeof(*e));
		e->v[0] = 0;
		e->v[1] = 0;
		SET_BCH_SB_ERROR_ENTRY_V2_ID(e, err);
		SET_BCH_SB_ERROR_ENTRY_V2_FIRST(e, now);
	}
	/* the setter saturates; a wrapped count would be zero, which validate rejects */
	SET_BCH_SB_ERROR_ENTRY_V2_NR(e, BCH_SB_ERROR_ENTRY_V2_NR(e) + 1);
	SET_BCH_SB_ERROR_ENTRY_V2_LAST(e, now);

	return bch2_trans_update(trans, &iter, &n->k_i, BTREE_UPDATE_internal_snapshot_node);
}

/*
 * For inode deletion: drop the damage key for this exact version, in the
 * same transaction as the inode key's deletion. Only this version's record
 * dies with it - damage at ancestor versions belongs to their inode
 * versions and is reaped when those are (or merged down by snapshot
 * deletion). Most inodes have no damage, so don't queue a deletion for a
 * key that isn't there:
 */
int bch2_damage_delete(struct btree_trans *trans, u64 inum, u32 snapshot)
{
	CLASS(btree_iter, iter)(trans, BTREE_ID_damage,
				SPOS(0, inum, snapshot),
				BTREE_ITER_all_snapshots);
	struct bkey_s_c k = bkey_try(bch2_btree_iter_peek_slot(&iter));

	/* whiteouts (cleared damage) die with the inode too: */
	if (k.k->type != KEY_TYPE_damage &&
	    k.k->type != KEY_TYPE_whiteout)
		return 0;

	return bch2_btree_delete_at(trans, &iter, BTREE_UPDATE_internal_snapshot_node);
}

/*
 * Clear the damage record for @inum in the calling subvolume's view: a
 * filtered delete, so it's a real delete when no older version needs the
 * record and a whiteout when one does - snapshots keep their view, and
 * the whiteout hides inherited damage from this view and its
 * descendants. Clearing a clean file is a no-op.
 */
int bch2_damage_clear(struct btree_trans *trans, subvol_inum inum)
{
	u32 snapshot;
	try(bch2_subvolume_get_snapshot(trans, inum.subvol, &snapshot));

	CLASS(btree_iter, iter)(trans, BTREE_ID_damage,
				SPOS(0, inum.inum, snapshot),
				BTREE_ITER_intent);
	struct bkey_s_c k = bkey_try(bch2_btree_iter_peek_slot(&iter));

	if (k.k->type != KEY_TYPE_damage)
		return 0;

	return bch2_btree_delete_at(trans, &iter, 0);
}

/*
 * Runtime data damage - loss or corruption found outside fsck_err()
 * reporting (device removal dropping the last replica, read errors):
 * count the sb error and record against the inode. Only extents btree
 * positions name an inum; an indirect extent's damage is counted but
 * unattributed - finding its inodes would mean walking reflink
 * pointers backwards.
 */
int bch2_damage_record_data_loss(struct btree_trans *trans, enum btree_id btree,
				 struct bpos pos, enum bch_sb_error_id err)
{
	bch2_sb_error_count(trans->c, err);

	return btree == BTREE_ID_extents
		? bch2_damage_record(trans, pos, err)
		: 0;
}

/*
 * A damage key has an inode at its exact position: records are written at
 * repaired keys' positions, which are inode keys themselves or content
 * keys - and a content key at snapshot X implies an inode at snapshot X.
 * The inode removal paths delete the two together; a damage key without
 * its inode was left behind by a kernel without the damage btree, and if
 * the inum is reused the stale key would claim the new file's history.
 */
static int check_damage_key(struct btree_trans *trans, struct btree_iter *iter,
			    struct bkey_s_c k)
{
	if (k.k->type != KEY_TYPE_damage)
		return 0;

	CLASS(btree_iter, inode_iter)(trans, BTREE_ID_inodes,
				      SPOS(0, k.k->p.offset, k.k->p.snapshot),
				      BTREE_ITER_all_snapshots);
	struct bkey_s_c inode_k = bkey_try(bch2_btree_iter_peek_slot(&inode_iter));

	CLASS(printbuf, buf)();
	if (ret_fsck_err_on(!bkey_is_inode(inode_k.k),
			    trans, damage_key_no_inode,
			    "damage key with no inode:\n%s",
			    (bch2_bkey_val_to_text(&buf, trans->c, k), buf.buf)))
		return bch2_btree_delete_at(trans, iter, BTREE_UPDATE_internal_snapshot_node);

	return 0;
}

int bch2_check_damage(struct bch_fs *c)
{
	CLASS(btree_trans, trans)(c);
	return for_each_btree_key_commit(trans, iter, BTREE_ID_damage, POS_MIN,
					 BTREE_ITER_all_snapshots|BTREE_ITER_prefetch, k,
					 NULL, NULL, BCH_TRANS_COMMIT_no_enospc,
					 check_damage_key(trans, &iter, k));
}

/*
 * Merge two damage keys' entries into a new key at @pos: the sorted-by-id
 * union, occurrence counts saturating-summed, last-occurrence times maxed.
 * For delete_dead_snapshots collapsing a dying snapshot node's key into its
 * live child's - the dying key can hold counts recorded after the child's
 * key was created, so plain copy-if-empty would lose them.
 *
 * Entries past the bkey size limit are dropped (lowest error ids kept),
 * matching bch2_damage_record()'s skip-on-full policy.
 */
struct bkey_i *bch2_damage_keys_merge(struct btree_trans *trans, struct bpos pos,
				      struct bkey_s_c a, struct bkey_s_c b)
{
	struct bkey_s_c_damage da = bkey_s_c_to_damage(a);
	struct bkey_s_c_damage db = bkey_s_c_to_damage(b);
	unsigned nr_a = bkey_val_bytes(a.k) / sizeof(da.v->entries[0]);
	unsigned nr_b = bkey_val_bytes(b.k) / sizeof(db.v->entries[0]);
	unsigned max_entries = (BKEY_VAL_U64s_MAX * sizeof(u64)) /
		sizeof(da.v->entries[0]);

	struct bkey_i_damage *n = bch2_trans_kmalloc(trans, sizeof(*n) +
			min(nr_a + nr_b, max_entries) * sizeof(da.v->entries[0]));
	if (IS_ERR(n))
		return ERR_CAST(n);

	bkey_damage_init(&n->k_i);
	n->k.p = pos;

	unsigned i = 0, j = 0, out = 0;
	while ((i < nr_a || j < nr_b) && out < max_entries) {
		const bch_sb_field_error_entry_v2 *ea = i < nr_a ? &da.v->entries[i] : NULL;
		const bch_sb_field_error_entry_v2 *eb = j < nr_b ? &db.v->entries[j] : NULL;
		bch_sb_field_error_entry_v2 *e = &n->v.entries[out++];

		unsigned id_a = ea ? BCH_SB_ERROR_ENTRY_V2_ID(ea) : UINT_MAX;
		unsigned id_b = eb ? BCH_SB_ERROR_ENTRY_V2_ID(eb) : UINT_MAX;

		if (id_a == id_b) {
			e->v[0] = 0;
			e->v[1] = 0;
			SET_BCH_SB_ERROR_ENTRY_V2_ID(e, id_a);
			SET_BCH_SB_ERROR_ENTRY_V2_NR(e,
				BCH_SB_ERROR_ENTRY_V2_NR(ea) +
				BCH_SB_ERROR_ENTRY_V2_NR(eb));
			SET_BCH_SB_ERROR_ENTRY_V2_FIRST(e,
				min(BCH_SB_ERROR_ENTRY_V2_FIRST(ea),
				    BCH_SB_ERROR_ENTRY_V2_FIRST(eb)));
			SET_BCH_SB_ERROR_ENTRY_V2_LAST(e,
				max(BCH_SB_ERROR_ENTRY_V2_LAST(ea),
				    BCH_SB_ERROR_ENTRY_V2_LAST(eb)));
			i++;
			j++;
		} else if (id_a < id_b) {
			*e = *ea;
			i++;
		} else {
			*e = *eb;
			j++;
		}
	}

	set_bkey_val_bytes(&n->k, out * sizeof(n->v.entries[0]));
	return &n->k_i;
}

/*
 * Does this inode have recorded damage, in @snapshot or an ancestor
 * version? The readdir filter: > 0 yes, 0 no, < 0 error. Everything about
 * how damage is stored stays behind this.
 */
int bch2_inode_has_damage(struct btree_trans *trans, u64 inum, u32 snapshot)
{
	CLASS(btree_iter, iter)(trans, BTREE_ID_damage,
				SPOS(0, inum, snapshot), BTREE_ITER_nopreserve);
	struct bkey_s_c k = bkey_try(bch2_btree_iter_peek_slot(&iter));

	return k.k->type == KEY_TYPE_damage;
}

/*
 * The union of an inode's damage across a snapshot and all its ancestors:
 * damage recorded against an ancestor version happened to the data a
 * descendant sees. Callers pass the view's snapshot (a subvolume's, for a
 * file the user has open).
 *
 * @out stays sorted by id; accumulating into a non-empty list is a merge -
 * counts sum, first-occurrence times min, last max.
 */
int bch2_damage_accumulate(struct btree_trans *trans, u64 inum, u32 snapshot,
			   bch_sb_errors_cpu *out)
{
	struct bch_fs *c = trans->c;

	do {
		CLASS(btree_iter, iter)(trans, BTREE_ID_damage,
				SPOS(0, inum, snapshot), BTREE_ITER_nopreserve);
		struct bkey_s_c k = bkey_try(bch2_btree_iter_peek_slot(&iter));

		if (k.k->type != KEY_TYPE_damage)
			break;

		struct bkey_s_c_damage d = bkey_s_c_to_damage(k);
		unsigned nr = bkey_val_bytes(k.k) / sizeof(d.v->entries[0]);

		for (unsigned i = 0; i < nr; i++) {
			const bch_sb_field_error_entry_v2 *e = &d.v->entries[i];
			unsigned eid	= BCH_SB_ERROR_ENTRY_V2_ID(e);
			u64 first	= BCH_SB_ERROR_ENTRY_V2_FIRST(e);
			u64 last	= BCH_SB_ERROR_ENTRY_V2_LAST(e);

			unsigned j = 0;
			while (j < out->nr && out->data[j].id < eid)
				j++;
			if (j < out->nr && out->data[j].id == eid) {
				out->data[j].nr += BCH_SB_ERROR_ENTRY_V2_NR(e);
				out->data[j].first_error_time =
					min(out->data[j].first_error_time, first);
				out->data[j].last_error_time =
					max(out->data[j].last_error_time, last);
				continue;
			}
			try(darray_insert_item(out, j,
				((struct bch_sb_error_entry_cpu) {
					.id			= eid,
					.nr			= BCH_SB_ERROR_ENTRY_V2_NR(e),
					.first_error_time	= first,
					.last_error_time	= last,
				})));
		}

		snapshot = bch2_snapshot_parent(c, k.k->p.snapshot);
	} while (snapshot);

	return 0;
}
