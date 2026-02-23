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
#include "posix_to_bcachefs.h"
#include "rust_shims.h"

struct bch_csum rust_csum_vstruct_sb(struct bch_sb *sb)
{
	struct nonce nonce = { 0 };

	return csum_vstruct(NULL, BCH_SB_CSUM_TYPE(sb), nonce, sb);
}

int rust_fmt_build_fs(struct bch_fs *c, const char *src_path)
{
	struct copy_fs_state s = {};
	int src_fd = open(src_path, O_RDONLY|O_NOATIME);
	if (src_fd < 0)
		return -errno;

	int ret = copy_fs(c, &s, src_fd, src_path);
	close(src_fd);
	return ret;
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

/* copy_fs shim for migrate — constructs copy_fs_state from flat parameters */

int rust_migrate_copy_fs(struct bch_fs *c,
			 int src_fd,
			 const char *fs_path,
			 u64 bcachefs_inum,
			 dev_t dev,
			 struct range *extent_array,
			 size_t nr_extents,
			 u64 reserve_start)
{
	ranges extents = {};

	for (size_t i = 0; i < nr_extents; i++)
		darray_push(&extents, extent_array[i]);

	struct copy_fs_state s = {
		.bcachefs_inum	= bcachefs_inum,
		.dev		= dev,
		.extents	= extents,
		.type		= BCH_MIGRATE_migrate,
		.reserve_start	= reserve_start,
	};

	BUG_ON(!s.reserve_start);

	return copy_fs(c, &s, src_fd, fs_path);
}

/* Open a block device without blkid probe (for migrate, not format) */

int rust_bdev_open(struct dev_opts *dev, blk_mode_t mode)
{
	dev->file = bdev_file_open_by_path(dev->path, mode, dev, NULL);
	int ret = PTR_ERR_OR_ZERO(dev->file);
	if (ret < 0)
		return ret;
	dev->bdev = file_bdev(dev->file);
	return 0;
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

int rust_write_data(struct bch_fs *c,
		    u64 inum, u64 offset,
		    const void *buf, size_t len,
		    u32 subvol, u32 replicas,
		    s64 *sectors_delta)
{
	struct bch_write_op op;
	struct bio_vec bv[RUST_IO_MAX / PAGE_SIZE];

	BUG_ON(offset	& (block_bytes(c) - 1));
	BUG_ON(len	& (block_bytes(c) - 1));
	BUG_ON(len > RUST_IO_MAX);

	bio_init(&op.wbio.bio, NULL, bv, ARRAY_SIZE(bv), 0);
	bch2_bio_map(&op.wbio.bio, (void *) buf, len);

	struct bch_inode_opts opts;
	bch2_inode_opts_get(c, &opts, false);

	bch2_write_op_init(&op, c, opts);
	op.write_point	= writepoint_hashed(0);
	op.nr_replicas	= replicas;
	op.subvol	= subvol;
	op.pos		= SPOS(inum, offset >> 9, U32_MAX);
	op.flags	|= BCH_WRITE_sync;

	int ret = bch2_disk_reservation_get(c, &op.res, len >> 9,
					    replicas, 0);
	if (ret) {
		*sectors_delta = 0;
		return ret;
	}

	closure_call(&op.cl, bch2_write, NULL, NULL);

	*sectors_delta = op.i_sectors_delta;
	return op.error;
}

static void rust_read_endio(struct bio *bio)
{
	closure_put(bio->bi_private);
}

int rust_read_data(struct bch_fs *c,
		   u64 inum, u32 subvol,
		   u64 offset,
		   void *buf, size_t len)
{
	BUG_ON(offset	& (block_bytes(c) - 1));
	BUG_ON(len	& (block_bytes(c) - 1));
	BUG_ON(len > RUST_IO_MAX);

	struct closure cl;
	closure_init_stack(&cl);

	struct bch_read_bio rbio;
	struct bio_vec bv[RUST_IO_MAX / PAGE_SIZE];

	bio_init(&rbio.bio, NULL, bv, ARRAY_SIZE(bv), 0);
	rbio.bio.bi_opf		= REQ_OP_READ|REQ_SYNC;
	rbio.bio.bi_iter.bi_sector	= offset >> 9;
	rbio.bio.bi_private		= &cl;
	bch2_bio_map(&rbio.bio, buf, len);

	struct bch_inode_unpacked inode;
	subvol_inum si = { .subvol = subvol, .inum = inum };
	int ret = bch2_inode_find_by_inum(c, si, &inode);
	if (ret)
		return ret;

	struct bch_inode_opts opts;
	bch2_inode_opts_get_inode(c, &inode, &opts);

	closure_get(&cl);
	bch2_read(c, rbio_init(&rbio.bio, c, opts, rust_read_endio), si);
	closure_sync(&cl);

	return rbio.ret;
}
