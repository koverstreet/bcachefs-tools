/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_READ_H
#define _BCACHEFS_BTREE_READ_H

#include "btree/bkey.h"
#include "btree/bkey/ops_gen.h"
#include "btree/trigger_gen.h"
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

bool bch2_btree_flush_all_reads(struct bch_fs *);

#endif /* _BCACHEFS_BTREE_READ_H */
