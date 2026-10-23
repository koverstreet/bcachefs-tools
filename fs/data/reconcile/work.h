/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_RECONCILE_WORK_H
#define _BCACHEFS_RECONCILE_WORK_H

#include "data/compress.h"
#include "alloc/disk_groups.h"

#include "data/reconcile/work_gen.h"
#include "data/reconcile/scan_gen.h"

extern const char * const bch2_reconcile_work_ids[];

static inline void bch2_reconcile_wakeup(struct bch_fs *c)
{
	c->reconcile.kick++;
	guard(rcu)();
	struct task_struct *p = rcu_dereference(c->reconcile.thread);
	if (p)
		wake_up_process(p);
}

static inline int bch2_reconcile_pending_wakeup(struct bch_fs *c)
{
	return bch2_set_reconcile_needs_scan(c,
		(struct reconcile_scan) { .type = RECONCILE_SCAN_pending}, true);
}

int bch2_extent_reconcile_pending_mod(struct btree_trans *, struct btree_iter *,
				      unsigned, struct bkey_s_c, bool);

void bch2_reconcile_status_to_text(struct printbuf *, struct bch_fs *);
void bch2_reconcile_scan_pending_to_text(struct printbuf *, struct bch_fs *);

void bch2_reconcile_stop(struct bch_fs *);
int bch2_reconcile_start(struct bch_fs *);

void bch2_fs_reconcile_exit(struct bch_fs *);
int bch2_fs_reconcile_init(struct bch_fs *);

#endif /* _BCACHEFS_RECONCILE_WORK_H */
