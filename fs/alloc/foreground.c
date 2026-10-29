// SPDX-License-Identifier: GPL-2.0
/*
 * Copyright 2012 Google, Inc.
 */

/* DOC(foreground-allocator)
 *
 * bcachefs automatically reduces fragmentation by segregating different types
 * of IO into separate buckets. Data from different files, metadata, and
 * internal bookkeeping each get their own write points (active allocation
 * contexts). The key insight: data written at the same time by the same file
 * tends to be deleted at the same time, so grouping it together means entire
 * buckets become free at once rather than becoming fragmented.
 *
 * User data write points are hashed by inode number, so different files
 * naturally land in different buckets without any user configuration. Separate
 * write points also exist for btree nodes, copygc, and the reconcile
 * subsystem, keeping internal IO from mixing with user data.
 */

#include "bcachefs.h"

#include "alloc/background.h"
#include "alloc/backpointers.h"
#include "alloc/buckets.h"
#include "alloc/check.h"
#include "alloc/discard.h"
#include "alloc/disk_groups.h"
#include "alloc/foreground.h"

#include "btree/iter.h"
#include "btree/update.h"
#include "btree/check.h"

#include "data/ec/create.h"
#include "data/nocow_locking.h"
#include "data/write.h"

#include "init/dev.h"
#include "init/error.h"

#include "journal/journal.h"
#include "journal/reclaim.h"

#include "sb/counters.h"
#include "sb/errors.h"

#include "util/clock.h"

#include <linux/math64.h>
#include <linux/rculist.h>
#include <linux/rcupdate.h>
#include <linux/sched/signal.h>

const char * const bch2_watermarks[] = {
#define x(t) #t,
	BCH_WATERMARKS()
#undef x
	NULL
};

void bch2_alloc_sectors_append_ptrs(struct bch_fs *c, struct write_point *wp,
				    struct bkey_i *k, unsigned sectors,
				    bool cached)
{
	bch2_alloc_sectors_append_ptrs_inlined(c, wp, k, sectors, cached);
}

/*
 * Append pointers to the space we just allocated to @k, and mark @sectors space
 * as allocated out of @ob
 */
void bch2_alloc_sectors_done(struct bch_fs *c, struct write_point *wp)
{
	bch2_alloc_sectors_done_inlined(c, wp);
}
