/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BKEY_METHODS_H
#define _BCACHEFS_BKEY_METHODS_H

#include "btree/bkey.h"

#include "btree/bkey_methods_gen.h"

extern const char * const bch2_bkey_types[];
extern const struct bkey_ops bch2_bkey_null_ops;

extern const struct bkey_ops bch2_bkey_ops[];

static inline const struct bkey_ops *bch2_bkey_type_ops(enum bch_bkey_type type)
{
	return likely(type < KEY_TYPE_MAX)
		? &bch2_bkey_ops[type]
		: &bch2_bkey_null_ops;
}

int bch2_bkey_val_validate(struct bch_fs *, struct bkey_s_c,
			   const struct bkey_validate_context *);
int __bch2_bkey_validate(struct bch_fs *, struct bkey_s_c,
			 const struct bkey_validate_context *);
int bch2_bkey_validate(struct bch_fs *, struct bkey_s_c,
		       const struct bkey_validate_context *);
void bch2_val_to_text(struct printbuf *, struct bch_fs *,
		      struct bkey_s_c);

void bch2_bkey_swab_val(const struct bch_fs *c, struct bkey_s);

static inline bool bch2_bkey_maybe_mergable(const struct bkey *l, const struct bkey *r)
{
	return l->type == r->type &&
		!bversion_cmp(l->bversion, r->bversion) &&
		bpos_eq(l->p, bkey_start_pos(r));
}

bool bch2_bkey_merge(struct bch_fs *, struct bkey_s, struct bkey_s_c);

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

void bch2_bkey_renumber(enum btree_node_type, struct bkey_packed *, int);

void __bch2_bkey_compat(const struct bch_fs *, unsigned, enum btree_id, unsigned, unsigned,
			int, struct bkey_format *, struct bkey_packed *);

static inline void bch2_bkey_compat(const struct bch_fs *c, unsigned level, enum btree_id btree_id,
			       unsigned version, unsigned big_endian,
			       int write,
			       struct bkey_format *f,
			       struct bkey_packed *k)
{
	if (unlikely(version < bcachefs_metadata_version_current) ||
	    big_endian != CPU_BIG_ENDIAN ||
	    IS_ENABLED(CONFIG_BCACHEFS_DEBUG))
		__bch2_bkey_compat(c, level, btree_id, version,
				   big_endian, write, f, k);

}

#endif /* _BCACHEFS_BKEY_METHODS_H */
