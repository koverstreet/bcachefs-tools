// SPDX-License-Identifier: GPL-2.0

#include "bcachefs.h"

#include "alloc/buckets.h"
#include "alloc/disk_groups.h"
#include "alloc/replicas.h"

#include "btree/cache.h"
#include "btree/iter.h"

#include "sb/members.h"
#include "sb/io.h"

#include "init/dev.h"
#include "init/error.h"
#include "init/passes.h"
#include "init/progress.h"

/*
 * Per member "range has btree nodes" bitmap:
 *
 * This is so that if we ever have to run the btree node scan to repair we don't
 * have to scan full devices:
 */

static bool __bch2_dev_btree_bitmap_marked(struct bch_fs *c, struct bkey_s_c k, bool with_gc)
{
	guard(rcu)();
	bkey_for_each_ptr(bch2_bkey_ptrs_c(k), ptr) {
		struct bch_dev *ca = bch2_dev_rcu_noerror(c, ptr->dev);
		if (ca &&
		    !__bch2_dev_btree_bitmap_marked_sectors(ca, ptr->offset, btree_sectors(c), with_gc))
			return false;
	}
	return true;
}

bool bch2_dev_btree_bitmap_marked(struct bch_fs *c, struct bkey_s_c k)
{
	return __bch2_dev_btree_bitmap_marked(c, k, true);
}

bool bch2_dev_btree_bitmap_marked_nogc(struct bch_fs *c, struct bkey_s_c k)
{
	return __bch2_dev_btree_bitmap_marked(c, k, false);
}

static void __bch2_dev_btree_bitmap_mark(struct bch_dev *ca,
					 struct bch_sb_field_members_v2 *mi,
					 u64 start, unsigned sectors, struct sb_write *w)
{
	struct bch_member *m = __bch2_members_v2_get_mut(mi, ca->dev_idx);

	u64 end = start + sectors;

	int resize = ilog2(roundup_pow_of_two(end)) - (m->btree_bitmap_shift + 6);
	if (resize > 0) {
		u64 old_bitmap = le64_to_cpu(m->btree_allocated_bitmap);
		u64 new_bitmap = 0;
		u64 new_gc_bitmap = 0;

		for (unsigned i = 0; i < 64; i++) {
			if (old_bitmap & BIT_ULL(i))
				new_bitmap |= BIT_ULL(i >> resize);
			if (ca->btree_allocated_bitmap_gc & BIT_ULL(i))
				new_gc_bitmap |= BIT_ULL(i >> resize);
		}

		m->btree_allocated_bitmap = cpu_to_le64(new_bitmap);
		m->btree_bitmap_shift += resize;
		sb_dirty(w);

		ca->btree_allocated_bitmap_gc = new_gc_bitmap;
	}

	BUG_ON(m->btree_bitmap_shift >= BCH_MI_BTREE_BITMAP_SHIFT_MAX);
	BUG_ON(end > 64ULL << m->btree_bitmap_shift);

	for (unsigned bit = start >> m->btree_bitmap_shift;
	     (u64) bit << m->btree_bitmap_shift < end;
	     bit++) {
		__le64 b = cpu_to_le64(BIT_ULL(bit));

		if (!(m->btree_allocated_bitmap & b)) {
			m->btree_allocated_bitmap |= b;
			sb_dirty(w);
		}

		ca->btree_allocated_bitmap_gc |= BIT_ULL(bit);
	}
}

void bch2_dev_btree_bitmap_mark_locked(struct bch_fs *c, struct bkey_s_c k, struct sb_write *w)
{
	struct bch_sb_field_members_v2 *mi = bch2_sb_field_get(c->disk_sb.sb, members_v2);

	guard(rcu)();
	bkey_for_each_ptr(bch2_bkey_ptrs_c(k), ptr) {
		struct bch_dev *ca = bch2_dev_rcu_noerror(c, ptr->dev);
		if (!ca)
			continue;

		__bch2_dev_btree_bitmap_mark(ca, mi, ptr->offset, btree_sectors(c), w);
	}
}

void bch2_dev_btree_bitmap_mark(struct bch_fs *c, struct bkey_s_c k)
{
	guard(mutex_noio)(&c->sb_lock);
	CLASS(sb_write, w)(c);
	bch2_dev_btree_bitmap_mark_locked(c, k, &w);
}

static int btree_bitmap_gc_btree_level(struct btree_trans *trans,
				       struct progress_indicator *progress,
				       enum btree_id btree, unsigned level)
{
	struct bch_fs *c = trans->c;
	CLASS(btree_node_iter, iter)(trans, btree, POS_MIN, 0, level, BTREE_ITER_prefetch);

	try(for_each_btree_key_continue(trans, iter, 0, k, ({
		if (!bch2_dev_btree_bitmap_marked(c, k))
			bch2_dev_btree_bitmap_mark(c, k);

		bch2_progress_update_iter(trans, progress, &iter);
	})));

	return 0;
}

int bch2_btree_bitmap_gc(struct bch_fs *c)
{
	bch2_progress_init(&c->recovery.progress, __func__, c, 0, ~0ULL);

	scoped_guard(mutex_noio, &c->sb_lock) {
		guard(rcu)();
		for_each_member_device_rcu(c, ca, NULL)
			ca->btree_allocated_bitmap_gc = 0;
	}

	{
		CLASS(btree_trans, trans)(c);

		for (unsigned btree = 0; btree < btree_id_nr_alive(c); btree++) {
			for (unsigned level = 1; level < BTREE_MAX_DEPTH; level++)
				try(btree_bitmap_gc_btree_level(trans, &c->recovery.progress, btree, level));

			CLASS(btree_node_iter, iter)(trans, btree, POS_MIN, 0,
						     bch2_btree_id_root(c, btree)->b->c.level, 0);
			struct btree *b;
			try(lockrestart_do(trans, PTR_ERR_OR_ZERO(b = bch2_btree_iter_peek_node(&iter))));

			if (!bch2_dev_btree_bitmap_marked(c, bkey_i_to_s_c(&b->key)))
				bch2_dev_btree_bitmap_mark(c, bkey_i_to_s_c(&b->key));
		}
	}

	u64 sectors_marked_old = 0, sectors_marked_new = 0;

	scoped_guard(mutex_noio, &c->sb_lock) {
		struct bch_sb_field_members_v2 *mi = bch2_sb_field_get(c->disk_sb.sb, members_v2);

		scoped_guard(rcu)
			for_each_member_device_rcu(c, ca, NULL) {
				sectors_marked_old += hweight64(ca->mi.btree_allocated_bitmap) << ca->mi.btree_bitmap_shift;
				sectors_marked_new += hweight64(ca->btree_allocated_bitmap_gc) << ca->mi.btree_bitmap_shift;

				struct bch_member *m = __bch2_members_v2_get_mut(mi, ca->dev_idx);
				m->btree_allocated_bitmap = cpu_to_le64(ca->btree_allocated_bitmap_gc);
			}
		bch2_write_super(c);
	}

	CLASS(printbuf, buf)();
	prt_str(&buf, "mi_btree_bitmap sectors ");
	prt_human_readable_u64(&buf, sectors_marked_old << 9);
	prt_str(&buf, " -> ");
	prt_human_readable_u64(&buf, sectors_marked_new << 9);
	bch_info(c, "%s", buf.buf);

	return 0;
}

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
