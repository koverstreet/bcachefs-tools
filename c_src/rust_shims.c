// SPDX-License-Identifier: GPL-2.0

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>

#include "libbcachefs.h"
#include "libbcachefs/journal/read.h"
#include "libbcachefs/journal/seq_blacklist.h"
#include "libbcachefs/sb/io.h"
#include "libbcachefs/sb/members.h"
#include "libbcachefs/alloc/buckets_types.h"
#include "libbcachefs/data/checksum.h"
#include "libbcachefs/data/read.h"
#include "libbcachefs/data/write.h"
#include "libbcachefs/btree/read.h"
#include "libbcachefs/init/error.h"
#include "libbcachefs/init/fs.h"
#include "libbcachefs/fs/inode.h"
#include "libbcachefs/journal/journal.h"
#include "libbcachefs/sb/clean.h"
#include "libbcachefs/alloc/foreground.h"
#include "libbcachefs/btree/update.h"
#include "libbcachefs/data/extents.h"
#include "libbcachefs/alloc/accounting.h"
#include "rust_shims.h"

struct bch_csum rust_csum_vstruct_sb(struct bch_sb *sb)
{
	struct nonce nonce = { 0 };

	return csum_vstruct(NULL, BCH_SB_CSUM_TYPE(sb), nonce, sb);
}


void strip_fs_alloc(struct bch_fs *c)
{
	struct bch_sb_field_clean *clean = bch2_sb_field_get(c->disk_sb.sb, clean);
	struct jset_entry *entry = clean->start;

	unsigned u64s = clean->field.u64s;
	while (entry != vstruct_end(&clean->field)) {
		if (entry->type == BCH_JSET_ENTRY_btree_root &&
		    btree_id_is_alloc(entry->btree_id)) {
			clean->field.u64s -= jset_u64s(entry->u64s);
			memmove(entry,
				vstruct_next(entry),
				vstruct_end(&clean->field) - (void *) vstruct_next(entry));
		} else {
			entry = vstruct_next(entry);
		}
	}

	swap(u64s, clean->field.u64s);
	bch2_sb_field_resize(&c->disk_sb, clean, u64s);

	scoped_guard(percpu_write, &c->capacity.mark_lock) {
		kfree(c->replicas.entries);
		c->replicas.entries = NULL;
		c->replicas.nr = 0;
	}

	bch2_sb_field_resize(&c->disk_sb, replicas_v0, 0);
	bch2_sb_field_resize(&c->disk_sb, replicas, 0);

	for_each_online_member(c, ca, 0) {
		bch2_sb_field_resize(&c->disk_sb, journal, 0);
		bch2_sb_field_resize(&c->disk_sb, journal_v2, 0);
	}

	for_each_member_device(c, ca) {
		struct bch_member *m = bch2_members_v2_get_mut(c->disk_sb.sb, ca->dev_idx);
		SET_BCH_MEMBER_FREESPACE_INITIALIZED(m, false);
	}

	c->disk_sb.sb->features[0] |= cpu_to_le64(BIT_ULL(BCH_FEATURE_no_alloc_info));
}

void rust_strip_alloc_do(struct bch_fs *c)
{
	mutex_lock(&c->sb_lock);
	strip_fs_alloc(c);
	bch2_write_super(c);
	mutex_unlock(&c->sb_lock);
}

/* online member iteration shim */

struct bch_dev *rust_get_next_online_dev(struct bch_fs *c,
					 struct bch_dev *ca,
					 unsigned ref_idx)
{
	return bch2_get_next_online_dev(c, ca, ~0U, READ, ref_idx);
}

void rust_put_online_dev_ref(struct bch_dev *ca, unsigned ref_idx)
{
	enumerated_ref_put(&ca->io_ref[READ], ref_idx);
}

struct rust_journal_entries rust_collect_journal_entries(struct bch_fs *c)
{
	struct rust_journal_entries ret = { NULL, 0 };
	struct genradix_iter iter;
	struct journal_replay **_p;
	size_t count = 0;

	genradix_for_each(&c->journal_entries, iter, _p)
		if (*_p)
			count++;

	if (!count)
		return ret;

	ret.entries = malloc(count * sizeof(*ret.entries));
	if (!ret.entries)
		die("malloc");

	genradix_for_each(&c->journal_entries, iter, _p)
		if (*_p)
			ret.entries[ret.nr++] = *_p;

	return ret;
}

/* dump sanitize shims — wraps crypto operations for encrypted fs dumps */

int rust_jset_decrypt(struct bch_fs *c, struct jset *j)
{
	return bch2_encrypt(c, JSET_CSUM_TYPE(j), journal_nonce(j),
			    j->encrypted_start,
			    vstruct_end(j) - (void *) j->encrypted_start);
}

int rust_bset_decrypt(struct bch_fs *c, struct bset *i, unsigned offset)
{
	return bset_encrypt(c, i, offset);
}


/* Bitmap shim — set_bit is atomic (locked bitops) */

void rust_set_bit(unsigned long nr, unsigned long *addr)
{
	set_bit(nr, addr);
}

/* Device reference shims */

struct bch_dev *rust_dev_tryget_noerror(struct bch_fs *c, unsigned dev)
{
	return bch2_dev_tryget_noerror(c, dev);
}

void rust_dev_put(struct bch_dev *ca)
{
	bch2_dev_put(ca);
}

/*
 * Data IO shims — bridge Rust async IO to kernel closure-based completion.
 *
 * The mapping:
 *   Rust Future construction  ↔  write_op_init / bio setup
 *   Future::poll (first)      ↔  closure_call(bch2_write) / bch2_read
 *   Future::poll (Ready)      ↔  closure_sync completion
 *
 * Currently synchronous (complete on first "poll"). When the closure
 * subsystem moves to Rust async, these shims become native Futures
 * where closure completion drives the Waker.
 */

int rust_write_submit(struct bch_fs *c,
		      struct bch_write_op *op,
		      struct bio_vec *bvecs, unsigned nr_bvecs,
		      const void *buf, size_t len,
		      u64 inum, u64 offset,
		      u32 subvol, u32 replicas,
		      u64 new_i_size,
		      void (*end_io)(struct bch_write_op *))
{
	bio_init(&op->wbio.bio, NULL, bvecs, nr_bvecs, 0);
	bch2_bio_map(&op->wbio.bio, (void *) buf, len);

	struct bch_inode_opts opts;
	bch2_inode_opts_get(c, &opts, false);

	bch2_write_op_init(op, c, opts);
	op->end_io	= end_io;
	op->write_point	= writepoint_hashed(0);
	op->nr_replicas	= replicas;
	op->subvol	= subvol;
	op->pos		= SPOS(inum, offset >> 9, U32_MAX);
	op->new_i_size	= new_i_size;
	op->flags	|= BCH_WRITE_sync|BCH_WRITE_only_specified_devs;

	int ret = bch2_disk_reservation_get(c, &op->res, len >> 9,
					    replicas, 0);
	if (ret)
		return ret;

	closure_call(&op->cl, bch2_write, NULL, NULL);
	return 0;
}

void rust_read_submit(struct bch_fs *c,
		     struct bch_read_bio *rbio,
		     struct bio_vec *bvecs, unsigned nr_bvecs,
		     void *buf, size_t len,
		     u64 offset,
		     struct bch_inode_opts opts,
		     subvol_inum inum,
		     bio_end_io_t endio)
{
	bio_init(&rbio->bio, NULL, bvecs, nr_bvecs, 0);
	rbio->bio.bi_opf		= REQ_OP_READ|REQ_SYNC;
	rbio->bio.bi_iter.bi_sector	= offset >> 9;
	bio_add_virt_nofail(&rbio->bio, buf, len);

	struct bch_read_bio *r = rbio_init(&rbio->bio, c, opts, endio);
	BUG_ON(r->_state);
	r->subvol = inum.subvol;

	CLASS(btree_trans, trans)(c);
	bch2_read(trans, r, r->bio.bi_iter, inum, NULL, NULL,
		  BCH_READ_retry_if_stale|
		  BCH_READ_may_promote|
		  BCH_READ_user_mapped);
}


/*
 * Extent construction for migrate — creates bkey extents pointing at
 * existing on-disk data. Handles bucket boundary splitting, generation
 * numbers, disk reservations, and btree insertion.
 *
 * All byte offsets; returns 0 on success. Updates *sectors_delta with
 * the total sectors linked.
 */
int rust_link_data(struct bch_fs *c,
		   u64 dst_inum, s64 *sectors_delta,
		   u64 logical, u64 physical, u64 length)
{
	struct bch_dev *ca = c->devs[0];

	BUG_ON(logical	& (block_bytes(c) - 1));
	BUG_ON(physical & (block_bytes(c) - 1));
	BUG_ON(length	& (block_bytes(c) - 1));

	logical		>>= 9;
	physical	>>= 9;
	length		>>= 9;

	BUG_ON(physical + length > bucket_to_sector(ca, ca->mi.nbuckets));

	*sectors_delta = 0;

	while (length) {
		struct bkey_i_extent *e;
		BKEY_PADDED_ONSTACK(k, BKEY_EXTENT_VAL_U64s_MAX) k;
		u64 b = sector_to_bucket(ca, physical);
		struct disk_reservation res;
		unsigned sectors;
		int ret;

		sectors = min(ca->mi.bucket_size -
			      (physical & (ca->mi.bucket_size - 1)),
			      length);

		e = bkey_extent_init(&k.k);
		e->k.p.inode	= dst_inum;
		e->k.p.offset	= logical + sectors;
		e->k.p.snapshot	= U32_MAX;
		e->k.size	= sectors;
		bch2_bkey_append_ptr(c, &e->k_i, (struct bch_extent_ptr) {
					.offset = physical,
					.dev = 0,
					.gen = *bucket_gen(ca, b),
				  });

		ret = bch2_disk_reservation_get(c, &res, sectors, 1,
						BCH_DISK_RESERVATION_NOFAIL);
		if (ret)
			return ret;

		CLASS(btree_trans, trans)(c);

		struct bch_inode_opts opts;
		ret = commit_do(trans, &res, NULL, 0, ({
			bch2_bkey_get_io_opts(trans, NULL, bkey_i_to_s_c(&e->k_i), &opts) ?:
			bch2_bkey_set_needs_reconcile(trans, NULL, &opts, &e->k_i,
						      SET_NEEDS_RECONCILE_opt_change, 0) ?:
			bch2_btree_insert(c, BTREE_ID_extents, &e->k_i, &res, 0, 0);
		}));
		bch2_disk_reservation_put(c, &res);

		if (ret)
			return ret;

		*sectors_delta	+= sectors;
		logical		+= sectors;
		physical	+= sectors;
		length		-= sectors;
	}

	return 0;
}

/* Accounting read shim */

void rust_accounting_mem_read(struct bch_fs *c, struct bpos p,
			      u64 *v, unsigned nr)
{
	bch2_accounting_mem_read(c, p, v, nr);
}
