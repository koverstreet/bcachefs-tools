// SPDX-License-Identifier: GPL-2.0

#include "bcachefs.h"

#include "alloc/buckets.h"

#include "sb/members.h"

#include "init/passes.h"

static void bch2_maybe_schedule_btree_bitmap_gc_work(struct work_struct *work)
{
	struct bch_fs *c = container_of(work, struct bch_fs, maybe_schedule_btree_bitmap_gc.work);

	if (bch2_recovery_pass_want_ratelimit(c, BCH_RECOVERY_PASS_btree_bitmap_gc, 1000))
		return;

	CLASS(bch_log_msg_level, msg)(c, 5);
	msg.m.suppress = true; /* run_explicit_recovery_pass will unsuppress */

	bool want_schedule = false;
	for_each_member_device(c, ca) {
		struct bch_dev_usage u;
		bch2_dev_usage_read_fast(ca, &u);

		u64 btree_sectors = bucket_to_sector(ca, u.buckets[BCH_DATA_btree]);
		u64 bitmap_sectors = hweight64(ca->mi.btree_allocated_bitmap) << ca->mi.btree_bitmap_shift;

		if (btree_sectors * 4 < bitmap_sectors) {
			prt_printf(&msg.m, "%s has ", ca->name);
			prt_human_readable_u64(&msg.m, btree_sectors << 9);
			prt_printf(&msg.m, " btree buckets and ");
			prt_human_readable_u64(&msg.m, bitmap_sectors << 9);
			prt_printf(&msg.m, " marked in bitmap\n");
			want_schedule = true;
		}
	}

	if (want_schedule)
		bch2_run_explicit_recovery_pass(c, &msg.m,
			BCH_RECOVERY_PASS_btree_bitmap_gc,
			RUN_RECOVERY_PASS_ratelimit);

	queue_delayed_work(system_long_wq, &c->maybe_schedule_btree_bitmap_gc, HZ * 60 * 60 * 24);
}

void bch2_maybe_schedule_btree_bitmap_gc_stop(struct bch_fs *c)
{
	cancel_delayed_work_sync(&c->maybe_schedule_btree_bitmap_gc);
}

void bch2_maybe_schedule_btree_bitmap_gc(struct bch_fs *c)
{
	INIT_DELAYED_WORK(&c->maybe_schedule_btree_bitmap_gc,
			  bch2_maybe_schedule_btree_bitmap_gc_work);
	bch2_maybe_schedule_btree_bitmap_gc_work(&c->maybe_schedule_btree_bitmap_gc.work);
}

/* For Rust: static inlines over percpu refs and RCU */
void rust_bch2_dev_put(struct bch_dev *ca)
{
	bch2_dev_put(ca);
}

void rust_bch2_dev_get(struct bch_dev *ca)
{
	bch2_dev_get(ca);
}

void rust_bch2_dev_get_outer(struct bch_dev *ca)
{
	bch2_dev_get_outer(ca);
}

void rust_bch2_dev_put_outer(struct bch_dev *ca)
{
	bch2_dev_put_outer(ca);
}
