// SPDX-License-Identifier: GPL-2.0
/*
 * Assorted bcachefs debug code - the debugfs files are debug.rs's
 *
 * Copyright 2010, 2011 Kent Overstreet <kent.overstreet@gmail.com>
 * Copyright 2012 Google, Inc.
 */

#include "bcachefs.h"

#include "btree/bkey_methods.h"
#include "btree/cache.h"
#include "btree/read.h"

#include "data/extents.h"

#include "debug.h"

__cold void bch2_btree_node_ondisk_to_text(struct printbuf *out, struct bch_fs *c,
				    const struct btree *b)
{
	struct btree_node *n_ondisk = NULL;
	struct extent_ptr_decoded pick;
	struct bch_dev *ca;
	struct bio *bio = NULL;
	unsigned offset = 0;
	int ret;

	if (bch2_bkey_pick_read_device(c, bkey_i_to_s_c(&b->key), NULL, &pick, 0, 0) <= 0) {
		prt_printf(out, "error getting device to read from: invalid device\n");
		return;
	}

	ca = bch2_dev_get_ioref(c, pick.ptr.dev, READ,
			BCH_DEV_READ_REF_btree_node_ondisk_to_text);
	if (!ca) {
		prt_printf(out, "error getting device to read from: not online\n");
		return;
	}

	n_ondisk = kvmalloc(btree_buf_bytes(b), GFP_KERNEL);
	if (!n_ondisk) {
		prt_printf(out, "memory allocation failure\n");
		goto out;
	}

	bio = bio_alloc_bioset(ca->disk_sb.bdev,
			       buf_nr_bvecs(n_ondisk, btree_buf_bytes(b)),
			       REQ_OP_READ|REQ_META,
			       GFP_NOIO,
			       &c->btree.bio);
	bio->bi_iter.bi_sector	= pick.ptr.offset;
	bch2_bio_map(bio, n_ondisk, btree_buf_bytes(b));

	ret = submit_bio_wait(bio);
	if (ret) {
		prt_printf(out, "IO error reading btree node: %s\n", bch2_err_str(ret));
		goto out;
	}

	while (offset < btree_sectors(c)) {
		struct bset *i;
		struct nonce nonce;
		struct bch_csum csum;
		struct bkey_packed *k;
		unsigned sectors;

		if (!offset) {
			i = &n_ondisk->keys;

			if (!bch2_checksum_type_valid(c, BSET_CSUM_TYPE(i))) {
				prt_printf(out, "unknown checksum type at offset %u: %llu\n",
					   offset, BSET_CSUM_TYPE(i));
				goto out;
			}

			nonce = btree_nonce(i, offset << 9);
			csum = csum_vstruct(c, BSET_CSUM_TYPE(i), nonce, n_ondisk);

			if (bch2_crc_cmp(csum, n_ondisk->csum)) {
				prt_printf(out, "invalid checksum\n");
				goto out;
			}

			bset_encrypt(c, i, offset << 9);

			sectors = vstruct_sectors(n_ondisk, c->block_bits);
		} else {
			struct btree_node_entry *bne = (void *) n_ondisk + (offset << 9);

			i = &bne->keys;

			if (i->seq != n_ondisk->keys.seq)
				break;

			if (!bch2_checksum_type_valid(c, BSET_CSUM_TYPE(i))) {
				prt_printf(out, "unknown checksum type at offset %u: %llu\n",
					   offset, BSET_CSUM_TYPE(i));
				goto out;
			}

			nonce = btree_nonce(i, offset << 9);
			csum = csum_vstruct(c, BSET_CSUM_TYPE(i), nonce, bne);

			if (bch2_crc_cmp(csum, bne->csum)) {
				prt_printf(out, "invalid checksum");
				goto out;
			}

			bset_encrypt(c, i, offset << 9);

			sectors = vstruct_sectors(bne, c->block_bits);
		}

		prt_printf(out, "  offset %u version %u, journal seq %llu\n",
			   offset,
			   le16_to_cpu(i->version),
			   le64_to_cpu(i->journal_seq));
		offset += sectors;

		scoped_guard(printbuf_indent, out)
			for (k = i->start; k != vstruct_last(i); k = bkey_p_next(k)) {
				struct bkey u;

				bch2_bkey_val_to_text(out, c, bkey_disassemble(b, k, &u));
				prt_newline(out);
			}
	}
out:
	if (bio)
		bio_put(bio);
	kvfree(n_ondisk);
	enumerated_ref_put(&ca->io_ref[READ],
			   BCH_DEV_READ_REF_btree_node_ondisk_to_text);
}
