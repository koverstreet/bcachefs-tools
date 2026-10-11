/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_XATTR_H
#define _BCACHEFS_XATTR_H

#include "str_hash.h"

int bch2_xattr_validate(struct bch_fs *, struct bkey_s_c,
			const struct bkey_validate_context *);
void bch2_xattr_to_text(struct printbuf *, struct bch_fs *, struct bkey_s_c);

#define bch2_bkey_ops_xattr ((struct bkey_ops) {	\
	.key_validate	= bch2_xattr_validate,		\
	.val_to_text	= bch2_xattr_to_text,		\
	.min_val_size	= 8,				\
})

static inline unsigned xattr_val_u64s(unsigned name_len, unsigned val_len)
{
	return DIV_ROUND_UP(offsetof(struct bch_xattr, x_name_and_value) +
			    name_len + val_len, sizeof(u64));
}

#define xattr_val(_xattr)					\
	((void *) (_xattr)->x_name_and_value + (_xattr)->x_name_len)

#include "fs/xattr_gen.h"

#define X_SEARCH(_type, _name, _len) ((struct xattr_search_key)	\
	{ .type = _type, .name = QSTR_INIT(_name, _len) })

#ifndef NO_BCACHEFS_FS

extern const struct xattr_handler * const bch2_xattr_handlers[];

/* For listxattr - fs/xattr.c: */
const char *bch2_xattr_list_prefix(unsigned, struct dentry *);

#endif /* NO_BCACHEFS_FS */

#endif /* _BCACHEFS_XATTR_H */
