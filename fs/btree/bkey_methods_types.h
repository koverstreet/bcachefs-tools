/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_BKEY_METHODS_TYPES_H
#define _BCACHEFS_BTREE_BKEY_METHODS_TYPES_H

struct bch_fs;

struct btree;

struct btree_trans;

struct bkey;

enum btree_node_type;

/*
 * key_validate: checks validity of @k, returns 0 if good or -EINVAL if bad. If
 * invalid, entire key will be deleted.
 *
 * When invalid, error string is returned via @err. @rw indicates whether key is
 * being read or written; more aggressive checks can be enabled when rw == WRITE.
 */
struct bkey_ops {
	int		(*key_validate)(struct bch_fs *c, struct bkey_s_c k,
					const struct bkey_validate_context *from);
	void		(*val_to_text)(struct printbuf *, struct bch_fs *,
				       struct bkey_s_c);
	void		(*swab)(const struct bch_fs *, struct bkey_s);
	bool		(*key_merge)(struct bch_fs *, struct bkey_s, struct bkey_s_c);
	int		(*trigger)(struct btree_trans *, struct btree_trigger_op);
	int		(*check_repair)(struct btree_trans *, struct btree_iter *,
					enum btree_id, unsigned, struct bkey_s_c);
	void		(*compat)(enum btree_id id, unsigned version,
				  unsigned big_endian, int write,
				  struct bkey_s);

	/* Size of value type when first created: */
	unsigned	min_val_size;
};

#endif /* _BCACHEFS_BTREE_BKEY_METHODS_TYPES_H */
