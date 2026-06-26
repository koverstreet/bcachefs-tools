// SPDX-License-Identifier: GPL-2.0
#include "bcachefs.h"

#include "alloc/background.h"
#include "alloc/backpointers.h"
#include "alloc/check_data.h"

#include "btree/bset.h"
#include "btree/interior.h"
#include "btree/read.h"
#include "btree/update.h"

#include "data/checksum.h"
#include "data/ec/trigger.h"
#include "data/extents.h"

#include "init/error.h"
#include "init/recovery.h"

#include "journal/init.h"

#include "sb/members.h"

#include "util/enumerated_ref.h"

#include <linux/bio.h>

/*
 * Drop @dev's pointer from @k and write the result back. If dropping it leaves
 * a non-btree key with no readable data, it's replaced with an error key of
 * type @error.
 */
int bch2_bkey_drop_device_and_update(struct btree_trans *trans, enum btree_id btree,
				     unsigned level, struct bkey_s_c k, unsigned dev,
				     enum bch_key_type_errors error)
{
	struct bch_fs *c = trans->c;
	struct bkey_i *n = errptr_try(bch2_bkey_make_mut_noupdate(trans, k));

	bch2_bkey_drop_device(c, bkey_i_to_s(n), dev);

	/*
	 * A btree node pointer is part of the btree topology - updating its key
	 * is a node-key update, not a leaf-key write.
	 */
	if (bkey_is_btree_ptr(&n->k)) {
		if (bch2_bkey_can_read(c, bkey_i_to_s_c(n)))
			return bch2_btree_node_update_key_at_pos(trans, btree, level, n);

		/*
		 * No readable replica left: if the node is still cached we can
		 * rewrite it from memory, otherwise it's lost - delete the
		 * dangling pointer and queue topology repair.
		 */
		CLASS(btree_node_iter, node_iter)(trans, btree, n->k.p, level + 1, level - 1,
						  BTREE_ITER_nofill);
		struct btree *b = bch2_btree_iter_peek_node(&node_iter);
		int ret = PTR_ERR_OR_ZERO(b);
		if (!ret)
			return bch2_btree_node_rewrite(trans, &node_iter, b, 0, 0, 0);
		if (!bch2_err_matches(ret, BCH_ERR_no_btree_node_nofill))
			return ret;

		CLASS(btree_node_iter, del_iter)(trans, btree, n->k.p, 0, level,
						 BTREE_ITER_intent);
		try(bch2_btree_iter_traverse(&del_iter));
		try(bch2_btree_delete_at(trans, &del_iter, 0));

		/*
		 * Flag the topology error: the returned error rewinds recovery
		 * to run check_topology, which repairs the hole if it can.
		 */
		CLASS(printbuf, buf)();
		prt_printf(&buf, "btree node with no readable replicas, dropping pointer:\n");
		bch2_bkey_val_to_text(&buf, c, bkey_i_to_s_c(n));
		return bch2_btree_lost_data(c, &buf, btree);
	}

	if (!bch2_bkey_can_read(c, bkey_i_to_s_c(n)))
		bch2_set_bkey_error(c, n, error);

	CLASS(btree_node_iter, iter)(trans, btree, n->k.p, 0, level, BTREE_ITER_intent);
	return bch2_btree_iter_traverse(&iter) ?:
	       bch2_trans_update(trans, &iter, n, 0);
}

/*
 * returns 0 if we didn't find a bad checksum, and did no work
 * returns 1 if we dropped bad replica
 */
int bch2_kill_replica_if_checksum_bad(struct btree_trans *trans,
				 enum btree_id btree, unsigned level,
				 struct bkey_s_c extent,
				 enum btree_id o_btree, struct bkey_s_c extent2, unsigned dev)
{
	struct bch_fs *c = trans->c;
	struct bkey_ptrs_c ptrs = bch2_bkey_ptrs_c(extent);
	const union bch_extent_entry *entry;
	struct extent_ptr_decoded p;
	int ret = 0;

	bkey_for_each_ptr_decode(extent.k, ptrs, p, entry)
		if (p.ptr.dev == dev)
			goto found;
	BUG();
found:
	if (!bkey_is_btree_ptr(extent.k) && !p.crc.csum_type)
		return false;

	struct bch_dev *ca = bch2_dev_get_ioref(c, dev, READ,
				BCH_DEV_READ_REF_check_extent_checksums);
	if (!ca)
		return false;

	size_t bytes = bkey_is_btree_ptr(extent.k)
		? c->opts.btree_node_size
		: p.crc.compressed_size << 9;
	void *data_buf __free(kvfree) = kvmalloc(bytes, GFP_KERNEL);
	if (!data_buf) {
		enumerated_ref_put(&ca->io_ref[READ],
				   BCH_DEV_READ_REF_check_extent_checksums);
		return -ENOMEM;
	}

	struct bio *bio __free(bio_put) =
		bio_alloc(ca->disk_sb.bdev, buf_nr_bvecs(data_buf, bytes), REQ_OP_READ, GFP_KERNEL);

	CLASS(printbuf, buf)(); /* before first goto */

	bio->bi_iter.bi_sector = p.ptr.offset;
	bch2_bio_map(bio, data_buf, bytes);
	ret = submit_bio_wait(bio);
	if (ret)
		goto err;

	bool bad;

	if (bkey_is_btree_ptr(extent.k)) {
		struct btree_node *bn = data_buf;

		if (le64_to_cpu(bn->magic) != bset_magic(c)) {
			bad = true;
		} else if (bch2_checksum_type_valid(c, BSET_CSUM_TYPE(&bn->keys))) {
			struct nonce nonce = btree_nonce(&bn->keys, 0);
			struct bch_csum csum = csum_vstruct(c, BSET_CSUM_TYPE(&bn->keys),
							    nonce, bn);
			bad = bch2_crc_cmp(bn->csum, csum);
		} else {
			bad = false;
		}

		if (!bad && extent.k->type == KEY_TYPE_btree_ptr_v2)
			bad = le64_to_cpu(bn->keys.seq) !=
			      le64_to_cpu(bkey_s_c_to_btree_ptr_v2(extent).v->seq);
	} else {
		struct nonce nonce = extent_nonce(extent.k->bversion, p.crc);
		struct bch_csum csum = bch2_checksum(c, p.crc.csum_type, nonce,
						     data_buf, bytes);
		bad = bch2_crc_cmp(csum, p.crc.csum);
	}

	if (!bad)
		goto out;

	prt_printf(&buf, "duplicate extents pointing to same space on dev %u, "
		   "checksum bad or wrong btree node - dropping:\n", dev);
	bch2_btree_id_to_text(&buf, btree);
	prt_str(&buf, " ");
	bch2_bkey_val_to_text(&buf, c, extent);
	prt_newline(&buf);
	bch2_btree_id_to_text(&buf, o_btree);
	prt_str(&buf, " ");
	bch2_bkey_val_to_text(&buf, c, extent2);

	if (fsck_err(trans, dup_backpointer_to_bad_csum_extent, "%s", buf.buf))
		ret = bch2_bkey_drop_device_and_update(trans, btree, level, extent, dev,
						       KEY_TYPE_ERROR_double_allocation) ?: 1;
fsck_err:
out:
err:
	enumerated_ref_put(&ca->io_ref[READ],
			   BCH_DEV_READ_REF_check_extent_checksums);
	return ret;
}

struct ptrs_repair {
	u8	drop;
	u8	drop_stripe;
	u8	reset_gen;
};

static inline int drop_this_ptr(struct ptrs_repair *r, unsigned ptr_bit)
{
	r->drop |= ptr_bit;
	return 0;
}

static int bch2_check_fix_ptr(struct btree_trans *trans,
			      struct bkey_s_c k,
			      struct extent_ptr_decoded p,
			      const union bch_extent_entry *entry,
			      struct ptrs_repair *r,
			      unsigned ptr_bit)
{
	if (p.ptr.dev == BCH_SB_MEMBER_INVALID)
		return 0;

	struct bch_fs *c = trans->c;
	CLASS(printbuf, buf)();

	CLASS(bch2_dev_tryget_noerror, ca)(c, p.ptr.dev);
	if (!ca) {
		if (test_bit(p.ptr.dev, c->devs_removed.d)) {
			if (ret_fsck_err(trans, ptr_to_removed_device,
				     "pointer to removed device %u\n"
				     "while marking %s",
				     p.ptr.dev,
				     (printbuf_reset(&buf),
				      bch2_bkey_val_to_text(&buf, c, k), buf.buf)))
				return drop_this_ptr(r, ptr_bit);
		} else {
			if (ret_fsck_err(trans, ptr_to_invalid_device,
				     "pointer to missing device %u\n"
				     "while marking %s",
				     p.ptr.dev,
				     (printbuf_reset(&buf),
				      bch2_bkey_val_to_text(&buf, c, k), buf.buf)))
				return drop_this_ptr(r, ptr_bit);
		}
		return 0;
	}

	struct bucket *g = PTR_GC_BUCKET(ca, &p.ptr);
	if (!g) {
		if (ret_fsck_err(trans, ptr_to_invalid_device,
			     "pointer to invalid bucket on device %u\n"
			     "while marking %s",
			     p.ptr.dev,
			     (printbuf_reset(&buf),
			      bch2_bkey_val_to_text(&buf, c, k), buf.buf)))
			return drop_this_ptr(r, ptr_bit);
		return 0;
	}

	enum bch_data_type data_type = bch2_bkey_ptr_data_type(k, p, entry);

	if (ret_fsck_err_on(!g->gen_valid,
			trans, ptr_to_missing_alloc_key,
			"bucket %u:%zu data type %s ptr gen %u missing in alloc btree\n"
			"while marking %s",
			p.ptr.dev, PTR_BUCKET_NR(ca, &p.ptr),
			bch2_data_type_str(ptr_data_type(k.k, &p.ptr)),
			p.ptr.generation,
			(printbuf_reset(&buf),
			 bch2_bkey_val_to_text(&buf, c, k), buf.buf))) {
		if (p.ptr.cached)
			return drop_this_ptr(r, ptr_bit);

		g->gen_valid		= true;
		g->generation			= p.ptr.generation;
	}

	/* g->gen_valid == true */

	if (ret_fsck_err_on(gen_cmp(p.ptr.generation, g->generation) > 0,
			trans, ptr_gen_newer_than_bucket_gen,
			"bucket %u:%zu data type %s ptr gen in the future: %u > %u\n"
			"while marking %s",
			p.ptr.dev, PTR_BUCKET_NR(ca, &p.ptr),
			bch2_data_type_str(ptr_data_type(k.k, &p.ptr)),
			p.ptr.generation, g->generation,
			(printbuf_reset(&buf),
			 bch2_bkey_val_to_text(&buf, c, k), buf.buf))) {
		if (p.ptr.cached)
			return drop_this_ptr(r, ptr_bit);

		/* XXX: if it's a data pointer, read it and see if it's good */
		r->reset_gen |= ptr_bit;
	}

	if (!p.ptr.cached) {
		if (ret_fsck_err_on(gen_cmp(p.ptr.generation, g->generation) < 0,
				trans, stale_dirty_ptr,
				"bucket %u:%zu data type %s stale dirty ptr: %u < %u\n"
				"while marking %s",
				p.ptr.dev, PTR_BUCKET_NR(ca, &p.ptr),
				bch2_data_type_str(ptr_data_type(k.k, &p.ptr)),
				p.ptr.generation, g->generation,
				(printbuf_reset(&buf),
				 bch2_bkey_val_to_text(&buf, c, k), buf.buf))) {
			/* XXX: if it's a data pointer, read it and see if it's good */
			r->reset_gen |= ptr_bit;
		}
	} else {
		if (ret_fsck_err_on(gen_cmp(g->generation, p.ptr.generation) > BUCKET_GC_GEN_MAX,
				trans, ptr_gen_newer_than_bucket_gen,
				"bucket %u:%zu gen %u data type %s: ptr gen %u too stale\n"
				"while marking %s",
				p.ptr.dev, PTR_BUCKET_NR(ca, &p.ptr), g->generation,
				bch2_data_type_str(ptr_data_type(k.k, &p.ptr)),
				p.ptr.generation,
				(printbuf_reset(&buf),
				 bch2_bkey_val_to_text(&buf, c, k), buf.buf)))
			return drop_this_ptr(r, ptr_bit);
	}

	if (data_type != BCH_DATA_btree && p.ptr.generation != g->generation)
		return 0;

	if (ret_fsck_err_on(bucket_data_type_mismatch(g->data_type, data_type),
			trans, ptr_bucket_data_type_mismatch,
			"bucket %u:%zu gen %u different types of data in same bucket: %s, %s\n"
			"while marking %s",
			p.ptr.dev, PTR_BUCKET_NR(ca, &p.ptr), g->generation,
			bch2_data_type_str(g->data_type),
			bch2_data_type_str(data_type),
			(printbuf_reset(&buf),
			 bch2_bkey_val_to_text(&buf, c, k), buf.buf))) {
		if (g->data_type == BCH_DATA_journal) {
			try(bch2_dev_journal_bucket_delete(ca, PTR_BUCKET_NR(ca, &p.ptr)));
			g->data_type		= data_type;
			g->stripe_sectors	= 0;
			g->dirty_sectors	= 0;
			g->cached_sectors	= 0;
			return 0;
		}

		if (!p.ptr.cached && data_type == BCH_DATA_btree &&
		    g->data_type == BCH_DATA_sb) {
			bch_err(c, "btree and superblock in the same bucket - cannot repair");
			return bch_err_throw(c, fsck_repair_unimplemented);
		}

		g->data_type = BCH_DATA_multiple;
	}

	if (p.has_ec) {
		struct gc_stripe *m = genradix_ptr(&c->ec.gc_stripes, p.ec.idx);

		if (ret_fsck_err_on(!m || !m->alive,
				trans, ptr_to_missing_stripe,
				"pointer to nonexistent stripe %llu\n"
				"while marking %s",
				(u64) p.ec.idx,
				(printbuf_reset(&buf),
				 bch2_bkey_val_to_text(&buf, c, k), buf.buf)) ||
		    ret_fsck_err_on(m && m->alive && !bch2_ptr_matches_stripe_m(m, p),
				trans, ptr_to_incorrect_stripe,
				"pointer does not match stripe %llu\n"
				"while marking %s",
				(u64) p.ec.idx,
				(printbuf_reset(&buf),
				 bch2_bkey_val_to_text(&buf, c, k), buf.buf)))
			r->drop_stripe |= ptr_bit;
	}

	return 0;
}

static int bch2_no_valid_pointers_repair(struct btree_trans *trans,
					 enum btree_id btree, struct bkey_s_c *k)
{
	struct bch_fs *c = trans->c;
	struct bkey_i *new =
		errptr_try(bch2_trans_kmalloc(trans, BKEY_EXTENT_U64s_MAX * sizeof(u64)));
	bkey_reassemble(new, *k);
	*k = bkey_i_to_s_c(new);

	bool found_good_cached_pointer = false;
	scoped_guard(rcu) {
		/*
		 * We can only flip a pointer from cached -> dirty
		 * without contortions here, when we're also repairing
		 * alloc info - to do this at runtime we'd have to pin
		 * the bucket with an open_bucket
		 */

		bkey_for_each_ptr(bch2_bkey_ptrs(bkey_i_to_s(new)), ptr) {
			struct bch_dev *ca;
			if (ptr->cached &&
			    (ca = bch2_dev_rcu_noerror(c, ptr->dev)) &&
			     !dev_ptr_stale_rcu(ca, ptr)) {
				ptr->cached = false;
				found_good_cached_pointer = true;
			}
		}
	}

	CLASS(printbuf, buf)();
	bch2_bkey_val_to_text(&buf, c, *k);

	if (found_good_cached_pointer) {
		ret_fsck_err(trans, extent_ptrs_all_invalid_but_cached,
			     "extent without valid dirty pointers\n%s", buf.buf);

		struct bch_inode_opts opts;
		try(bch2_bkey_get_io_opts(trans, NULL, *k, &opts));
		try(bch2_bkey_set_needs_reconcile(trans, NULL, &opts, bkey_i_to_s(new),
						  BKEY_EXTENT_U64s_MAX,
						  SET_NEEDS_RECONCILE_opt_change, 0));
	} else {
		ret_fsck_err(trans, extent_ptrs_all_invalid,
			     "extent without valid pointers\n%s", buf.buf);
		bch2_set_bkey_error(c, new, KEY_TYPE_ERROR_no_valid_pointers_repair);
	}

	CLASS(btree_node_iter, iter)(trans, btree, new->k.p, 0, 0,
				     BTREE_ITER_intent|BTREE_ITER_all_snapshots);

	try(bch2_btree_iter_traverse(&iter));
	try(bch2_trans_update(trans, &iter, new,
			      BTREE_UPDATE_internal_snapshot_node|
			      BTREE_TRIGGER_norun));
	return 0;
}

int bch2_check_fix_ptrs(struct btree_trans *trans, struct btree_iter *iter,
			enum btree_id btree, unsigned level, struct bkey_s_c k)
{
	struct bch_fs *c = trans->c;

	/* We don't yet do btree key updates correctly for when we're RW */
	BUG_ON(test_bit(BCH_FS_rw, &c->flags));

	if (!bkey_is_btree_ptr(k.k) &&
	    !bch2_bkey_can_read(c, k))
		try(bch2_no_valid_pointers_repair(trans, btree, &k));

	struct ptrs_repair r = {};

	struct bkey_ptrs_c ptrs_c = bch2_bkey_ptrs_c(k);
	const union bch_extent_entry *entry_c;
	struct extent_ptr_decoded p;
	unsigned ptr_bit = 1;

	bkey_for_each_ptr_decode(k.k, ptrs_c, p, entry_c) {
		try(bch2_check_fix_ptr(trans, k, p, entry_c, &r, ptr_bit));
		ptr_bit <<= 1;
	}

	if (r.drop ||
	    r.drop_stripe ||
	    r.reset_gen) {
		struct bkey_i *new =
			errptr_try(bch2_trans_kmalloc(trans, BKEY_EXTENT_U64s_MAX * sizeof(u64)));
		bkey_reassemble(new, k);

		struct bkey_ptrs ptrs = bch2_bkey_ptrs(bkey_i_to_s(new));
		if (r.reset_gen) {
			unsigned ptr_bit = 1;
			guard(rcu)();
			bkey_for_each_ptr(ptrs, ptr) {
				if (r.reset_gen & ptr_bit) {
					struct bch_dev *ca = bch2_dev_rcu_noerror(c, ptr->dev);
					if (ca)
						ptr->generation = PTR_GC_BUCKET(ca, ptr)->generation;
				}
				ptr_bit <<= 1;
			}
		}

		if (r.drop_stripe)
			bch2_bkey_drop_ec_mask(c, new, r.drop_stripe);

		if (r.drop)
			bch2_bkey_drop_ptrs_mask(c, new, r.drop);

		struct bch_inode_opts opts;
		try(bch2_bkey_get_io_opts(trans, NULL, k, &opts));
		try(bch2_bkey_set_needs_reconcile(trans, NULL, &opts, bkey_i_to_s(new),
						  BKEY_EXTENT_U64s_MAX,
						  SET_NEEDS_RECONCILE_opt_change, 0));
		if (bkey_is_btree_ptr(&new->k))
			trans->extra_disk_res = (u64) bch2_bkey_nr_ptrs_allocated(c, bkey_i_to_s_c(new)) *
				btree_sectors(c);

		if (!level) {
			try(bch2_trans_update(trans, iter, new,
					      BTREE_UPDATE_internal_snapshot_node|
					      BTREE_TRIGGER_norun));
		} else {
			CLASS(btree_node_iter, node_iter)(trans, btree, k.k->p,
							  0, level - 1, BTREE_ITER_intent);
			struct btree *b = errptr_try(bch2_btree_iter_peek_node(&node_iter));

			return bch2_btree_node_update_key(trans, &node_iter, b, new,
							  BCH_TRANS_COMMIT_no_enospc, false) ?:
				bch_err_throw(c, transaction_restart_commit);
		}
	}

	return 0;
}
