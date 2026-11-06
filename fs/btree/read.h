/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_READ_H
#define _BCACHEFS_BTREE_READ_H

#include "btree/bkey_methods.h"
#include "btree/bset.h"
#include "btree/locking.h"
#include "data/checksum.h"
#include "data/extents.h"
#include "init/error.h"

#include "btree/read_gen.h"

static inline unsigned btree_ptr_sectors_written(struct bkey_s_c k)
{
	return k.k->type == KEY_TYPE_btree_ptr_v2
		? le16_to_cpu(bkey_s_c_to_btree_ptr_v2(k).v->sectors_written)
		: 0;
}

static inline int bch2_bkey_in_btree_node(struct bch_fs *c, struct btree *b,
					  struct bkey_s_c k,
					  const struct bkey_validate_context *from)
{
	int ret = 0;

	bkey_fsck_err_on(bpos_lt(k.k->p, b->data->min_key),
			 c, bkey_before_start_of_btree_node,
			 "key before start of btree node");

	bkey_fsck_err_on(bpos_gt(k.k->p, b->data->max_key),
			 c, bkey_after_end_of_btree_node,
			 "key past end of btree node");
fsck_err:
	return ret;
}

void bch2_btree_node_wait_on_read(struct btree_trans *, struct btree *);
void bch2_btree_node_wait_on_write(struct btree_trans *, struct btree *);

DEFINE_GUARD(btree_node_io_lock, struct btree *,
	     bch2_btree_node_io_lock(_T),
	     bch2_btree_node_io_unlock(_T));

static inline struct nonce btree_nonce(struct bset *i, unsigned offset)
{
	return (struct nonce) {{
		[0] = cpu_to_le32(offset),
		[1] = ((__le32 *) &i->seq)[0],
		[2] = ((__le32 *) &i->seq)[1],
		[3] = ((__le32 *) &i->journal_seq)[0]^BCH_NONCE_BTREE,
	}};
}

static inline int bset_encrypt(struct bch_fs *c, struct bset *i, unsigned offset)
{
	struct nonce nonce = btree_nonce(i, offset);
	int ret;

	if (!offset) {
		struct btree_node *bn = container_of(i, struct btree_node, keys);
		unsigned bytes = (void *) &bn->keys - (void *) &bn->flags;

		ret = bch2_encrypt(c, BSET_CSUM_TYPE(i), nonce,
				   &bn->flags, bytes);
		if (ret)
			return ret;

		nonce = nonce_add(nonce, round_up(bytes, CHACHA_BLOCK_SIZE));
	}

	return bch2_encrypt(c, BSET_CSUM_TYPE(i), nonce, i->_data,
			    vstruct_end(i) - (void *) i->_data);
}

void bch2_btree_node_drop_keys_outside_node(struct btree *);

int bch2_validate_bset_keys(struct bch_fs *, struct bch_dev *,
			    struct btree *, struct bset *, int,
			    struct bch_io_failures *,
			    struct printbuf *);
int bch2_validate_bset(struct bch_fs *, struct bch_dev *,
		       struct btree *, struct bset *,
		       unsigned, int,
		       struct bch_io_failures *,
		       struct printbuf *);

int bch2_btree_node_read_done(struct bch_fs *, struct bch_dev *,
			      struct btree *,
			      struct bch_io_failures *,
			      struct printbuf *);
void bch2_btree_node_read(struct btree_trans *, struct btree *, bool);

void bch2_btree_read_bio_to_text(struct printbuf *, struct btree_read_bio *);

bool bch2_btree_flush_all_reads(struct bch_fs *);

/* A node header as a machine of @big_endian wrote it, made this one's, or back */
static inline void compat_btree_node(unsigned big_endian, struct btree_node *bn)
{
	if (big_endian != CPU_BIG_ENDIAN) {
		bch2_bpos_swab(&bn->min_key);
		bch2_bpos_swab(&bn->max_key);
	}
}

#endif /* _BCACHEFS_BTREE_READ_H */
