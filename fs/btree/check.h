/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_GC_H
#define _BCACHEFS_BTREE_GC_H

#include "btree/bkey.h"
#include "btree/check_gen.h"
#include "btree/node/bset_gen.h"
#include "btree/node/iter_gen.h"
#include "btree/path_gen.h"
#include "btree/types_gen.h"
#include "btree/node_gen.h"

int bch2_check_topology(struct bch_fs *);

#endif /* _BCACHEFS_BTREE_GC_H */
