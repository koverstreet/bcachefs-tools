/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BKEY_METHODS_H
#define _BCACHEFS_BKEY_METHODS_H

#include "btree/bkey.h"

#include "btree/bkey/ops_gen.h"
#include "btree/compat_gen.h"
#include "btree/trigger_gen.h"

static inline int bch2_key_trigger_old(struct btree_trans *trans,
			enum btree_id btree_id, unsigned level,
			struct bkey_s_c old,
			enum btree_iter_update_trigger_flags flags)
{
	struct bkey_i deleted;

	bkey_init(&deleted.k);
	deleted.k.p = old.k->p;

	return bch2_key_trigger(trans, (struct btree_trigger_op) {
		.btree		= btree_id,
		.level		= level,
		.old		= old,
		.new		= bkey_i_to_s(&deleted),
		.new_buf_u64s	= deleted.k.u64s,
		.flags		= BTREE_TRIGGER_overwrite|flags,
	});
}

static inline int bch2_key_trigger_new(struct btree_trans *trans,
			enum btree_id btree_id, unsigned level,
			struct bkey_s new, unsigned new_buf_u64s,
			enum btree_iter_update_trigger_flags flags)
{
	struct bkey_i deleted;

	bkey_init(&deleted.k);
	deleted.k.p = new.k->p;

	return bch2_key_trigger(trans, (struct btree_trigger_op) {
		.btree		= btree_id,
		.level		= level,
		.old		= bkey_i_to_s_c(&deleted),
		.new		= new,
		.new_buf_u64s	= new_buf_u64s,
		.flags		= BTREE_TRIGGER_insert|flags,
	});
}

#endif /* _BCACHEFS_BKEY_METHODS_H */
