/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_XATTR_H
#define _BCACHEFS_XATTR_H

#include "str_hash.h"
#include "fs/xattr_gen.h"

#define bch2_bkey_ops_xattr ((struct bkey_ops) {	\
	.key_validate	= bch2_xattr_validate,		\
	.val_to_text	= bch2_xattr_to_text,		\
})

#ifndef NO_BCACHEFS_FS

extern const struct xattr_handler * const bch2_xattr_handlers[];

#endif /* NO_BCACHEFS_FS */

#endif /* _BCACHEFS_XATTR_H */
