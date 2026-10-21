// SPDX-License-Identifier: GPL-2.0
/*
 * io_misc.c - fallocate: fpunch, truncate and finsert are io_misc.rs
 */

#include "bcachefs.h"

#include "alloc/buckets.h"
#include "alloc/foreground.h"

#include "btree/bkey_buf.h"
#include "btree/update.h"

#include "data/extents.h"
#include "data/io_misc.h"
#include "data/write.h"

#include "fs/inode.h"

#include "init/error.h"

#include "util/clock.h"

/* Overwrites whatever was present with zeroes: */
int bch2_extent_fallocate(struct btree_trans *trans,
			  subvol_inum inum,
			  struct btree_iter *iter,
			  u64 sectors,
			  struct bch_inode_opts opts,
			  s64 *i_sectors_delta,
			  struct write_point_specifier write_point)
{
	struct bch_fs *c = trans->c;
	CLASS(disk_reservation, res)(c);
	struct open_buckets open_buckets = { 0 };
	unsigned sectors_allocated = 0, new_replicas;
	bool unwritten = opts.nocow &&
	    c->sb.version >= bcachefs_metadata_version_unwritten_extents;
	int ret;

	struct bkey_buf new __cleanup(bch2_bkey_buf_exit);
	bch2_bkey_buf_init(&new);

	CLASS(closure_stack, cl)();

	struct bkey_s_c k = bkey_try(bch2_btree_iter_peek_slot(iter));

	sectors = min_t(u64, sectors, k.k->p.offset - iter->pos.offset);
	unsigned nr_overwritable = bch2_bkey_durability_safe(c, k).nr_overwritable;

	new_replicas = max(0, (int) opts.data_replicas - (int) nr_overwritable);

	/*
	 * Get a disk reservation before (in the nocow case) calling
	 * into the allocator:
	 */
	ret = bch2_disk_reservation_add(c, &res.r, sectors, new_replicas,
				       BCH_DISK_RESERVATION_PARTIAL);
	if (unlikely(!res.r.sectors && new_replicas)) {
		ret = bch_err_throw(c, ENOSPC_disk_reservation);
		goto err_noprint;
	}

	/* data_replicas, less however many the reservation couldn't give us: */
	unsigned nr_replicas = opts.data_replicas - (new_replicas - res.r.nr_replicas);

	if (new_replicas) {
		/*
		 * A partial reservation is whatever was left: on a full
		 * filesystem that can be less than a block per replica, and a
		 * key that size would be empty or misaligned:
		 */
		sectors = round_down(div_u64(res.r.sectors, res.r.nr_replicas),
				     block_sectors(c));
		if (unlikely(!sectors)) {
			ret = bch_err_throw(c, ENOSPC_disk_reservation);
			goto err_noprint;
		}
	}

	unsigned new_buf_u64s;
	if (!unwritten) {
		struct bkey_i_reservation *reservation;

		new_buf_u64s = sizeof(*reservation) / sizeof(u64);
		bch2_bkey_buf_realloc(&new, new_buf_u64s);
		reservation = bkey_reservation_init(new.k);
		reservation->k.p = iter->pos;
		bch2_key_resize(&reservation->k, sectors);
		reservation->v.nr_replicas = nr_replicas;
	} else {
		struct bkey_i_extent *e;
		struct bch_devs_list devs_have;
		struct write_point *wp;

		devs_have.nr = 0;

		new_buf_u64s = BKEY_EXTENT_U64s_MAX;
		bch2_bkey_buf_realloc(&new, new_buf_u64s);

		e = bkey_extent_init(new.k);
		e->k.p = iter->pos;

		struct alloc_request *req __free(alloc_request_put) = NULL;
		ret = PTR_ERR_OR_ZERO(req = alloc_request_get(trans,
						opts.foreground_target,
						false,
						&devs_have,
						nr_replicas,
						nr_replicas,
						BCH_WATERMARK_normal,
						0, &cl)) ?:
			bch2_alloc_sectors_req(trans, req, write_point, &wp);
		if (bch2_err_matches(ret, BCH_ERR_operation_blocked))
			ret = bch2_wait_on_allocator(trans, req, ret, &cl) ?:
				btree_trans_restart(trans, BCH_ERR_transaction_restart_nested);
		if (ret)
			goto err;

		sectors = min_t(u64, sectors, wp->sectors_free);
		sectors_allocated = sectors;

		bch2_key_resize(&e->k, sectors);

		bch2_open_bucket_get(c, wp, &open_buckets);
		bch2_alloc_sectors_append_ptrs(c, wp, &e->k_i, sectors, false);
		bch2_alloc_sectors_done(c, wp);

		extent_for_each_ptr(extent_i_to_s(e), ptr)
			ptr->unwritten = true;
	}

	ret = bch2_extent_update(trans, inum, iter, new.k, new_buf_u64s, &res.r,
				 0, i_sectors_delta, true, 0, NULL);
err:
	if (!ret && sectors_allocated)
		bch2_increment_clock(c, sectors_allocated, WRITE);
	if (should_print_err(ret)) {
		CLASS(printbuf, buf)();
		bch2_inum_offset_err_msg_trans(trans, &buf, inum.subvol, iter->pos);
		prt_printf(&buf, "fallocate error: %s", bch2_err_str(ret));
		bch_err_ratelimited(c, "%s", buf.buf);
	}
err_noprint:
	bch2_open_buckets_put(c, &open_buckets);

	return ret;
}
