// SPDX-License-Identifier: GPL-2.0

#include "bcachefs.h"

#include "alloc/accounting.h"
#include "alloc/disk_groups.h"

#include "btree/interior.h"
#include "btree/update.h"

#include "data/checksum.h"
#include "data/compress.h"
#include "data/extents.h"
#include "data/reconcile/trigger.h"
#include "data/reconcile/work.h"
#include "data/reflink.h"

#include "fs/inode.h"

#include "init/error.h"

struct bkey_s_c reconcile_bp_get_key(struct btree_trans *trans,
				     struct btree_iter *iter,
				     struct bkey_s_c_backpointer bp)
{
	struct bch_fs *c = trans->c;
	int ret = 0;
	CLASS(printbuf, buf)();

	/*
	 * we're still using fsck_err() here, which does a goto, which has
	 * problems with CLASS()
	 */
	CLASS(btree_iter_uninit, iter2)(trans);

	/* don't allow bps to non btree nodes: */
	if (fsck_err_on(!bp.v->level,
			trans, reconcile_bp_to_leaf_node_key,
			"reconcile bp to leaf node key\n%s",
			(bch2_bkey_val_to_text(&buf, c, bp.s_c), buf.buf))) {
		ret =   bch2_btree_delete(trans, BTREE_ID_reconcile_scan, bp.k->p, 0) ?:
			bch2_trans_commit(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc);
		return ret ? bkey_s_c_err(ret) : bkey_s_c_null;
	}

	bch2_trans_node_iter_init(trans, iter, bp.v->btree_id, bp.v->pos, 0, bp.v->level, 0);

	/* walk down a level - we need to have the node pointed to locked, not
	 * the parent node, for synchronization with btree_node_update_key when
	 * the node isn't yet written */

	bch2_trans_node_iter_init(trans, &iter2, bp.v->btree_id, bp.v->pos, 0, bp.v->level - 1, 0);
	struct btree *b = bch2_btree_iter_peek_node(&iter2);
	if (IS_ERR(b))
		return bkey_s_c_err(PTR_ERR(b));

	struct bkey_s_c k = bkey_s_c_null;
	if (b) {
		if (btree_node_will_make_reachable(b))
			return bkey_s_c_null;

		k = bkey_i_to_s_c(&b->key);
		if (bpos_eq(bp.k->p, bch2_bkey_get_reconcile_bp_pos(c, k)))
			return k;
	}

	prt_printf(&buf, "reconcile backpointer to missing/incorrect btree ptr\n");
	bch2_bkey_val_to_text(&buf, c, bp.s_c);
	prt_newline(&buf);
	if (k.k)
		bch2_bkey_val_to_text(&buf, c, k);
	else
		prt_str(&buf, "(no key)");

	if (b) {
		prt_newline(&buf);
		bch2_bkey_val_to_text(&buf, c, bkey_i_to_s_c(&b->key));
	}

	if (fsck_err(trans, reconcile_bp_to_missing_btree_ptr, "%s", buf.buf))
		ret =   bch2_btree_delete(trans, BTREE_ID_reconcile_scan, bp.k->p, 0) ?:
			bch2_trans_commit(trans, NULL, NULL, BCH_TRANS_COMMIT_no_enospc);
fsck_err:
	return ret ? bkey_s_c_err(ret) : bkey_s_c_null;
}

int bch2_bkey_get_io_opts(struct btree_trans *trans,
			  struct per_snapshot_io_opts *snapshot_opts, struct bkey_s_c k,
			  struct bch_inode_opts *opts)
{
	struct bch_fs *c = trans->c;
	enum io_opts_mode {
		IO_OPTS_metadata,
		IO_OPTS_reflink,
		IO_OPTS_user,
	} mode;

	if (bkey_is_btree_ptr(k.k))
		mode = IO_OPTS_metadata;
	else if (bkey_is_indirect(k.k))
		mode = IO_OPTS_reflink;
	else if (bkey_is_user_data(k.k) ||
		 k.k->type == KEY_TYPE_reflink_p) {
		mode = IO_OPTS_user;

		if (unlikely(!k.k->p.snapshot)) {
			CLASS(printbuf, buf)();
			bch2_bkey_val_to_text(&buf, trans->c, k);
			WARN(1, "user data key with snapshot == 0\n%s", buf.buf);
			bch2_inode_opts_get(c, opts, false);
			return 0;
		}
	} else {
		/* KEY_TYPE_error? */
		bch2_inode_opts_get(c, opts, false);
		return 0;
	}

	if (!snapshot_opts) {
		bch2_inode_opts_get(c, opts, mode == IO_OPTS_metadata);

		if (mode == IO_OPTS_user) {
			struct bch_inode_unpacked inode;
			int ret = bch2_inode_find_by_inum_snapshot(trans, k.k->p.inode, k.k->p.snapshot,
								   &inode, BTREE_ITER_cached);
			if (bch2_err_matches(ret, BCH_ERR_transaction_restart))
				return ret;
			if (!ret)
				bch2_inode_opts_get_inode(c, &inode, opts);
		}
	} else {
		/*
		 * If we have a per_snapshot_io_opts, we're doing a scan in
		 * natural key order: we can cache options for the inode number
		 * we're currently on, but we have to cache options from every
		 * different snapshot version of that inode
		 */

		bool metadata = mode == IO_OPTS_metadata;
		if (snapshot_opts->fs_io_opts.change_cookie	!= c->opt_change_cookie ||
		    snapshot_opts->metadata			!= metadata) {
			bch2_inode_opts_get(c, &snapshot_opts->fs_io_opts, metadata);

			snapshot_opts->metadata = metadata;
			snapshot_opts->cur_inum = 0;
			snapshot_opts->d.nr	= 0;
		}

		if (mode == IO_OPTS_user) {
			if (snapshot_opts->cur_inum != k.k->p.inode) {
				snapshot_opts->d.nr = 0;

				/*
				 * norestart: callers (e.g. fsck repair paths)
				 * may have updates queued; an internal
				 * trans_begin() would clobber them
				 */
				struct bkey_s_c inode_k;
				int ret = 0;
				for_each_btree_key_max_norestart(trans, iter, BTREE_ID_inodes,
						SPOS(0, k.k->p.inode, 0),
						SPOS(0, k.k->p.inode, U32_MAX),
						BTREE_ITER_all_snapshots, inode_k, ret) {
					if (!bkey_is_inode(inode_k.k))
						continue;

					struct bch_inode_unpacked inode;
					bch2_inode_unpack(c, inode_k, &inode);

					struct snapshot_io_opts_entry e = { .snapshot = inode_k.k->p.snapshot };
					bch2_inode_opts_get_inode(c, &inode, &e.io_opts);

					ret = darray_push(&snapshot_opts->d, e);
					if (ret)
						break;
				}
				if (ret)
					return ret;

				snapshot_opts->cur_inum	= k.k->p.inode;
				snapshot_opts->inum_scan_cookie	= false;
			}

			struct snapshot_io_opts_entry *i =
				darray_find_p(snapshot_opts->d, i,
					      bch2_snapshot_is_ancestor(trans, k.k->p.snapshot, i->snapshot));
			if (i) {
				*opts = i->io_opts;
				return 0;
			}
		}

		*opts = snapshot_opts->fs_io_opts;
	}

	const struct bch_extent_reconcile *old;
	if (mode == IO_OPTS_reflink &&
	    (old = bch2_bkey_reconcile_opts(c, k))) {
#define x(_name)								\
		if (old->_name##_from_inode)					\
			opts->_name		= old->_name;			\
		opts->_name##_from_inode	= old->_name##_from_inode;
		BCH_RECONCILE_OPTS()
#undef x

		/*
		 * On-disk reconcile_opts were written under whatever
		 * bch2_io_opts_fixups rules existed at the time. If the rules
		 * have grown since, the persisted opts may violate today's
		 * invariants - re-run fixups so consumers downstream of this
		 * read see a combination that's valid under the current rules.
		 */
		bch2_io_opts_fixups(opts);

		/*
		 * We shouldn't be able to get here: a checksum or compression
		 * type we don't know is an incompat feature, so a filesystem
		 * using one shouldn't have mounted at all. This is for when
		 * that goes wrong - a versioning mistake on our side - and the
		 * cost of being wrong isn't an error, it's a crash:
		 * bch2_io_opts_fixups() above doesn't range check, and the
		 * conversions in bch2_bkey_needs_reconcile() BUG() on an
		 * unknown csum opt and index out of bounds on an unknown
		 * compression opt.
		 *
		 * Fall back to the filesystem option for new writes, the way
		 * bch2_inode_opts_get_inode() does for inode opts.
		 */
		if (unlikely(opts->data_checksum >= BCH_CSUM_OPT_NR)) {
			opts->data_checksum = c->opts.data_checksum;
			opts->data_checksum_from_inode = false;
		}
		if (unlikely(!bch2_compression_opt_valid(opts->background_compression))) {
			opts->background_compression = c->opts.background_compression;
			opts->background_compression_from_inode = false;
		}
	}

	return 0;
}
