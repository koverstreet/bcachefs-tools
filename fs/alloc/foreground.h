/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_ALLOC_FOREGROUND_H
#define _BCACHEFS_ALLOC_FOREGROUND_H

#include "bcachefs.h"
#include "alloc/buckets.h"
#include "alloc/types_gen.h"
#include "btree/iter.h"
#include "data/extents.h"
#include "data/write_gen.h"
#include "sb/members.h"

#include <linux/hash.h>

#include "alloc/foreground_gen.h"

static inline struct bch_dev *ob_dev(struct bch_fs *c, struct open_bucket *ob)
{
	return bch2_dev_have_ref(c, ob->dev);
}

DEFINE_FREE(alloc_request_put, struct alloc_request *,
	    if (!IS_ERR_OR_NULL(_T)) alloc_request_put(_T))

static inline struct write_point_specifier writepoint_hashed(unsigned long v)
{
	return (struct write_point_specifier) { .v = v | 1 };
}

static inline struct write_point_specifier writepoint_ptr(struct write_point *wp)
{
	return (struct write_point_specifier) { .v = (unsigned long) wp };
}

#endif /* _BCACHEFS_ALLOC_FOREGROUND_H */
