// SPDX-License-Identifier: GPL-2.0
#ifndef NO_BCACHEFS_FS

#include "bcachefs.h"

#include "btree/bkey_buf.h"

#include "data/read.h"

#include "snapshots/snapshot.h"

#include "vfs/io.h"
#include "vfs/buffered.h"
#include "vfs/direct.h"
#include "vfs/pagecache.h"

#include <linux/backing-dev.h>
#include <linux/pagemap.h>
#include <linux/writeback.h>
#include <linux/version.h>

/*
 * readpage(s):
 *
 * A readahead bio covers the folios the readahead control asked for, plus
 * whatever readpage_bio_extend() adds to reach an extent boundary - partial
 * reads of a checksummed or compressed extent are expensive. Only that
 * extension is speculative, and only it may be abandoned.
 *
 * Abandoning it is worth doing because readpage_bio_extend() drops btree locks
 * to allocate folios: under write load we lose the relock repeatedly to an
 * extent that keeps moving, and holding the transaction open to chase it costs
 * more than the readahead is worth.
 *
 * The complication is that reads are sector granular and the page cache is
 * folio granular, so where we stop issuing is generally not a folio boundary.
 * Every folio has to be unlocked exactly once, by whoever owns it: us for the
 * folios we abandon, the read completion for the folios we issued. The folio
 * we stop inside belongs to the completion - IO is landing in it - but it
 * isn't uptodate, and folio_end_read() can't express that. bch_folio.
 * partially_uptodate does.
 */

static void bch2_readpages_end_io(struct bio *bio)
{
	struct bch_read_bio *rbio = to_rbio(bio);
	struct folio_iter fi;

	/* a stale partially_uptodate would spin do_read_cache_folio() forever: */
	struct folio *last = page_folio(bio->bi_io_vec[bio->bi_vcnt - 1].bv_page);
	struct folio *partial = bch2_folio(last)->partially_uptodate ? last : NULL;

	bio_for_each_folio_all(fi, bio)
		folio_end_read(fi.folio, !rbio->ret && fi.folio != partial);

	bio_put(bio);
}

static void readpage_bio_drop_unissued(struct bch_read_bio *rbio)
{
	struct bio *bio = &rbio->bio;
	struct bvec_iter iter;
	struct folio_vec fv;
	unsigned keep = bio->bi_iter.bi_idx;
#if LINUX_VERSION_CODE < KERNEL_VERSION(7, 3, 0)
	bool partial = bio->bi_iter.bi_bvec_done != 0;
#else
	bool partial = bio->bi_iter.bi_offset != 0;
#endif

	if (partial) {
		struct folio *folio = page_folio(bio->bi_io_vec[keep].bv_page);

#if LINUX_VERSION_CODE < KERNEL_VERSION(7, 3, 0)
		bch2_folio(folio)->partially_uptodate = bio->bi_iter.bi_bvec_done >> 9;
#else
		bch2_folio(folio)->partially_uptodate = bio->bi_iter.bi_offset >> 9;
#endif
		keep++;
	}

	bio_for_each_folio(fv, bio, iter) {
		if (partial) {
			partial = false;
			continue;
		}

		folio_end_read(fv.fv_folio, false);
	}

	bio->bi_vcnt		= keep;
	bio->bi_iter.bi_size	= 0;
}

struct readpages_iter {
	struct address_space	*mapping;
	unsigned		idx;
	folios			folios;
};

static inline void readpages_iter_folio_revert(struct readahead_control *ractl,
					       struct folio *folio)
{
	bch2_folio_release(folio);
	ractl->_nr_pages += folio_nr_pages(folio);
	ractl->_index -= folio_nr_pages(folio);
}

static int readpages_iter_init(struct readpages_iter *iter,
			       struct readahead_control *ractl)
{
	struct folio *folio;

	*iter = (struct readpages_iter) { ractl->mapping };

	while ((folio = __readahead_folio(ractl))) {
		if (!bch2_folio_create(folio, GFP_KERNEL) ||
		    darray_push(&iter->folios, folio)) {
			readpages_iter_folio_revert(ractl, folio);
			return iter->folios.nr ? 0 : -ENOMEM;
		}

		folio_put(folio);
	}

	return 0;
}

static void readpages_iter_exit(struct readpages_iter *iter,
			        struct readahead_control *ractl)
{
	darray_for_each_reverse(iter->folios, folio) {
		readpages_iter_folio_revert(ractl, *folio);
		folio_get(*folio);
	}
}

static inline struct folio *readpage_iter_peek(struct readpages_iter *iter)
{
	if (iter->idx >= iter->folios.nr)
		return NULL;
	return iter->folios.data[iter->idx];
}

static inline void readpage_iter_advance(struct readpages_iter *iter)
{
	iter->idx++;
}

static bool extent_partial_reads_expensive(const struct bch_fs *c, struct bkey_s_c k)
{
	struct bkey_ptrs_c ptrs = bch2_bkey_ptrs_c(k);
	struct bch_extent_crc_unpacked crc;
	const union bch_extent_entry *i;

	bkey_for_each_crc(k.k, ptrs, crc, i)
		if (crc.csum_type || crc.compression_type)
			return true;
	return false;
}

static inline struct folio *__readpage_alloc_folio(gfp_t gfp, unsigned order)
{
#if LINUX_VERSION_CODE < KERNEL_VERSION(6,19,0)
	return filemap_alloc_folio(gfp, order);
#else
	return filemap_alloc_folio(gfp, order, NULL);
#endif
}

/*
 * Not allocate_dropping_locks(): that tries GFP_NOWAIT first, and GFP_NOWAIT
 * is not the mapping's mask minus blocking - it's missing __GFP_MOVABLE, and
 * page cache folios have to be movable or they pin pageblocks against
 * compaction. Drop only the ability to block.
 */
static struct folio *readpage_alloc_folio(struct btree_trans *trans,
					  struct address_space *mapping,
					  unsigned order)
{
	gfp_t gfp = readahead_gfp_mask(mapping);

	struct folio *folio = __readpage_alloc_folio(gfp & ~__GFP_DIRECT_RECLAIM, order);
	if (!folio) {
		bch2_trans_unlock(trans);
		folio = __readpage_alloc_folio(gfp, order);
	}

	return folio;
}

noinline
static int readpage_bio_extend(struct btree_trans *trans,
			       struct readpages_iter *iter,
			       struct bio *bio,
			       unsigned sectors_this_extent,
			       bool get_more)
{
	while (bio_sectors(bio) < sectors_this_extent &&
	       bio->bi_vcnt < bio->bi_max_vecs) {
		struct folio *folio = readpage_iter_peek(iter);
		int ret;

		if (folio) {
			readpage_iter_advance(iter);
		} else {
			pgoff_t folio_offset = bio_end_sector(bio) >> PAGE_SECTORS_SHIFT;

			if (!get_more)
				break;

			unsigned sectors_remaining = sectors_this_extent - bio_sectors(bio);

			if (sectors_remaining < PAGE_SECTORS << mapping_min_folio_order(iter->mapping))
				break;

			unsigned order = ilog2(rounddown_pow_of_two(sectors_remaining) / PAGE_SECTORS);

			/* ensure proper alignment */
			order = min(order, __ffs(folio_offset|BIT(31)));

			/*
			 * mapping_max_folio_order() is 0 on !THP no matter what
			 * we asked for at inode init: without THP the MM has no
			 * split_folio(), only a stub that warns and returns
			 * -EINVAL, so a large folio we put in the page cache
			 * here can never be split again.
			 *
			 * This can't fight the mapping_min_folio_order() check
			 * above: a min order above 0 means bs > ps, and
			 * bch2_fs_alloc() already refuses to mount such a
			 * filesystem on !THP.
			 */
			order = min(order, mapping_max_folio_order(iter->mapping));

			folio = xa_load(&iter->mapping->i_pages, folio_offset);
			if (folio && !xa_is_value(folio))
				break;

			folio = readpage_alloc_folio(trans, iter->mapping, order);
			if (!folio)
				break;

			if (!allocate_dropping_locks_norelock(trans,
						__bch2_folio_create(folio, _gfp))) {
				folio_put(folio);
				break;
			}

			ret = allocate_dropping_locks_errcode_norelock(trans,
					filemap_add_folio(iter->mapping, folio, folio_offset, _gfp));
			if (ret) {
				__bch2_folio_release(folio);
				folio_put(folio);
				break;
			}

			folio_put(folio);
		}

		BUG_ON(folio_sector(folio) != bio_end_sector(bio));

		bio_add_folio_nofail(bio, folio, folio_size(folio), 0);
	}

	return bch2_trans_relock(trans);
}

static void bchfs_read(struct btree_trans *trans,
		       struct bch_read_bio *rbio,
		       subvol_inum inum,
		       struct readpages_iter *readpages_iter)
{
	struct bch_fs *c = trans->c;
	int flags = BCH_READ_retry_if_stale|
		BCH_READ_may_promote;
	int ret = 0;

	rbio->subvol = inum.subvol;

	struct folio *first = page_folio(rbio->bio.bi_io_vec[0].bv_page);
	u64 read_from = folio_sector(first) + bch2_folio(first)->partially_uptodate;

	/* the readahead window; past this is the speculative extension: */
	sector_t orig_end = readpages_iter && readpages_iter->folios.nr
		? folio_end_sector(readpages_iter->folios.data[readpages_iter->folios.nr - 1])
		: bio_end_sector(&rbio->bio);

	struct bkey_buf sk __cleanup(bch2_bkey_buf_exit);
	bch2_bkey_buf_init(&sk);

	bch2_trans_begin(trans);
	CLASS(btree_iter, iter)(trans, BTREE_ID_extents,
			     POS(inum.inum, rbio->bio.bi_iter.bi_sector),
			     BTREE_ITER_slots);
	while (1) {
		struct bkey_s_c k;
		unsigned bytes, sectors;
		s64 offset_into_extent;
		enum btree_id data_btree = BTREE_ID_extents;

		bch2_trans_begin(trans);

		u32 snapshot;
		ret = bch2_subvolume_get_snapshot(trans, inum.subvol, &snapshot);
		if (ret)
			goto err;

		bch2_btree_iter_set_snapshot(&iter, snapshot);

		bch2_btree_iter_set_pos(&iter,
				POS(inum.inum, rbio->bio.bi_iter.bi_sector));

		k = bch2_btree_iter_peek_slot(&iter);
		ret = bkey_err(k);
		if (ret)
			goto err;

		/*
		 * Extents entirely within the prefix a previous read already
		 * filled in: skip them, which leaves us extent aligned.
		 */
		if (k.k->p.offset <= read_from) {
			bio_advance(&rbio->bio,
				    (k.k->p.offset - iter.pos.offset) << 9);
			continue;
		}

		offset_into_extent = iter.pos.offset -
			bkey_start_offset(k.k);
		sectors = k.k->size - offset_into_extent;

		bch2_bkey_buf_reassemble(&sk, k);

		ret = bch2_read_indirect_extent(trans, &data_btree,
					&offset_into_extent, &sk);
		if (ret)
			goto err;

		k = bkey_i_to_s_c(sk.k);

		sectors = min_t(unsigned, sectors, k.k->size - offset_into_extent);

		if (readpages_iter &&
		    bio_sectors(&rbio->bio) < sectors &&
		    rbio->bio.bi_vcnt < rbio->bio.bi_max_vecs) {
			ret = readpage_bio_extend(trans, readpages_iter, &rbio->bio, sectors,
						  extent_partial_reads_expensive(c, k));
			if (ret)
				goto err;
		}

		bytes = min(sectors, bio_sectors(&rbio->bio)) << 9;
		swap(rbio->bio.bi_iter.bi_size, bytes);

		if (rbio->bio.bi_iter.bi_size == bytes)
			flags |= BCH_READ_last_fragment;
		else
			flags |= BCH_READ_must_clone;

		bch2_bio_page_state_set(c, &rbio->bio, k);

		ret = bch2_read_extent(trans, rbio, iter.pos,
				       data_btree, k, offset_into_extent, flags);

		/* On successful BCH_READ_last_fragment submit we no longer own rbio: */
		if (!ret && (flags & BCH_READ_last_fragment))
			break;

		swap(rbio->bio.bi_iter.bi_size, bytes);
err:
		if (bch2_err_matches(ret, BCH_ERR_transaction_restart)) {
			/* everything asked for is issued; the rest isn't worth a retry */
			if (readpages_iter &&
			    rbio->bio.bi_iter.bi_sector >= orig_end) {
				readpage_bio_drop_unissued(rbio);
				bio_endio(&rbio->bio);

				/* bch2_trans_put() won't take a restarted trans */
				bch2_trans_begin(trans);
				return;
			}

			flags &= ~BCH_READ_last_fragment;
			continue;
		}
		if (ret)
			break;

		bio_advance(&rbio->bio, bytes);
	}

	if (ret) {
		CLASS(printbuf, buf)();
		bch2_read_err_msg_trans(trans, &buf, rbio, iter.pos);
		prt_printf(&buf, "data read error: %s", bch2_err_str(ret));
		bch_err_ratelimited(c, "%s", buf.buf);

		rbio->ret = ret;
		bio_endio(&rbio->bio);
	}
}

void bch2_readahead(struct readahead_control *ractl)
{
	struct bch_inode_info *inode = to_bch_ei(ractl->mapping->host);
	struct bch_fs *c = inode->v.i_sb->s_fs_info;
	struct folio *folio;
	struct readpages_iter readpages_iter;
	struct blk_plug plug;

	struct bch_inode_opts opts;
	bch2_inode_opts_get_inode(c, &inode->ei_inode, &opts);

	int ret = readpages_iter_init(&readpages_iter, ractl);
	if (ret)
		return;

	/*
	 * Besides being a general performance optimization, plugging helps with
	 * avoiding btree transaction srcu warnings - submitting a bio can
	 * block, and we don't want todo that with the transaction locked.
	 *
	 * However, plugged bios are submitted when we schedule; we ideally
	 * would have our own scheduler hook to call unlock_long() before
	 * scheduling.
	 */
	blk_start_plug(&plug);
	if (!bch2_pagecache_add_tryget(inode)) {
		readpages_iter_exit(&readpages_iter, ractl);
		goto out;
	}

	struct btree_trans *trans = bch2_trans_get(c);
	while ((folio = readpage_iter_peek(&readpages_iter))) {
		unsigned n = min_t(unsigned,
				   readpages_iter.folios.nr -
				   readpages_iter.idx,
				   BIO_MAX_VECS);
		struct bch_read_bio *rbio =
			rbio_init(bio_alloc_bioset(NULL, n, REQ_OP_READ,
						   GFP_KERNEL, &c->bio_read),
				  c,
				  opts,
				  bch2_readpages_end_io);

		readpage_iter_advance(&readpages_iter);

		rbio->bio.bi_iter.bi_sector = folio_sector(folio);
		bio_add_folio_nofail(&rbio->bio, folio, folio_size(folio), 0);

		bchfs_read(trans, rbio, inode_inum(inode),
			   &readpages_iter);
		bch2_trans_unlock(trans);
	}
	bch2_trans_put(trans);

	bch2_pagecache_add_put(inode);
out:
	blk_finish_plug(&plug);
	darray_exit(&readpages_iter.folios);
}

static void bch2_read_single_folio_end_io(struct bio *bio)
{
	complete(bio->bi_private);
}

/*
 * The folios readpage_bio_extend() added after @folio are locked and ours to
 * finish, as bch2_readpages_end_io() does for readahead; @folio stays locked,
 * it's the caller's.
 */
static void bch2_read_single_folio_finish_extension(struct bch_read_bio *rbio,
						    struct folio *folio)
{
	struct bio *bio = &rbio->bio;
	struct folio_iter fi;

	struct folio *last = page_folio(bio->bi_io_vec[bio->bi_vcnt - 1].bv_page);
	struct folio *partial = bch2_folio(last)->partially_uptodate ? last : NULL;

	bio_for_each_folio_all(fi, bio)
		if (fi.folio != folio)
			folio_end_read(fi.folio, !rbio->ret && fi.folio != partial);
}

/*
 * Read one folio, synchronously; @folio is locked and stays locked.
 *
 * A partial read of a checksummed or compressed extent reads the whole extent
 * anyway. With @extend, the rest of it goes into the page cache, as readahead
 * does, rather than being thrown away: without it a sequential rewrite that
 * doesn't cover whole folios reads the same extent once per folio. The caller
 * must hold pagecache_add, since new folios are added.
 */
int bch2_read_single_folio(struct folio *folio, struct address_space *mapping,
			   bool extend)
{
	struct bch_inode_info *inode = to_bch_ei(mapping->host);
	struct bch_fs *c = inode->v.i_sb->s_fs_info;
	struct bch_read_bio *rbio;
	struct bch_inode_opts opts;
	struct blk_plug plug;
	int ret;
	DECLARE_COMPLETION_ONSTACK(done);

	BUG_ON(folio_test_uptodate(folio));
	BUG_ON(folio_test_dirty(folio));

	struct bch_folio *s = bch2_folio_create(folio, GFP_KERNEL);
	if (!s)
		return -ENOMEM;

	bch2_inode_opts_get_inode(c, &inode->ei_inode, &opts);

	unsigned nr_vecs = extend
		? min_t(unsigned, BIO_MAX_VECS,
			DIV_ROUND_UP(c->opts.encoded_extent_max, PAGE_SIZE) + 1)
		: 1;
	struct readpages_iter readpages_iter = { .mapping = mapping };

	rbio = rbio_init(bio_alloc_bioset(NULL, nr_vecs, REQ_OP_READ, GFP_KERNEL, &c->bio_read),
			 c,
			 opts,
			 bch2_read_single_folio_end_io);
	rbio->bio.bi_private = &done;
	rbio->bio.bi_opf = REQ_OP_READ|REQ_SYNC;
	rbio->bio.bi_iter.bi_sector = folio_sector(folio);
	bio_add_folio_nofail(&rbio->bio, folio, folio_size(folio), 0);

	blk_start_plug(&plug);
	bch2_trans_run(c, (bchfs_read(trans, rbio, inode_inum(inode),
				      extend ? &readpages_iter : NULL), 0));
	blk_finish_plug(&plug);
	wait_for_completion(&done);

	if (extend)
		bch2_read_single_folio_finish_extension(rbio, folio);

	ret = bch2_err_class(rbio->ret);
	bio_put(&rbio->bio);

	if (ret < 0)
		return ret;

	s->partially_uptodate = 0;
	folio_mark_uptodate(folio);
	return 0;
}

int bch2_read_folio(struct file *file, struct folio *folio)
{
	int ret;

	ret = bch2_read_single_folio(folio, folio->mapping, false);
	folio_unlock(folio);
	return bch2_err_class(ret);
}

#endif /* NO_BCACHEFS_FS */
