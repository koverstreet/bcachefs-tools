// SPDX-License-Identifier: GPL-2.0
#include "bcachefs.h"

#include "alloc/accounting.h"

#include "btree/bbpos.h"

#include "init/passes.h"
#include "init/progress.h"

static const char * const bch2_progress_units[] = {
#define x(n, v)	#n,
	BCH_PROGRESS_UNITS()
#undef x
	NULL
};

void bch2_progress_init(struct progress_indicator *s,
			const char *msg,
			struct bch_fs *c,
			u64 leaf_btree_id_mask,
			u64 inner_btree_id_mask)
{
	memset(s, 0, sizeof(*s));

	s->msg = msg ? strip_bch2(msg) : NULL;
	s->units = BCH_PROGRESS_UNITS_nodes;
	s->next_print = jiffies + HZ * 10;

	/* This is only an estimation: nodes can have different replica counts */
	const u32 expected_node_disk_sectors =
		READ_ONCE(c->opts.metadata_replicas) * btree_sectors(c);

	const u64 btree_id_mask = leaf_btree_id_mask | inner_btree_id_mask;

	for (unsigned i = 0; i < btree_id_nr_alive(c); i++) {
		if (!(btree_id_mask & BIT_ULL(i)))
			continue;

		struct disk_accounting_pos acc;
		disk_accounting_key_init(acc, btree, .id = i);

		struct {
			u64 disk_sectors;
			u64 total_nodes;
			u64 inner_nodes;
		} v = {0};
		bch2_accounting_mem_read(c, disk_accounting_pos_to_bpos(&acc),
			(u64 *)&v, sizeof(v) / sizeof(u64));

		/* Better to estimate as 0 than the total node count */
		if (inner_btree_id_mask & BIT_ULL(i))
			s->total += v.inner_nodes;

		if (!(leaf_btree_id_mask & BIT_ULL(i)))
			continue;

		/*
		 * We check for zeros to degrade gracefully when run
		 * with un-upgraded accounting info (missing some counters).
		 */
		if (v.total_nodes != 0)
			s->total += v.total_nodes - v.inner_nodes;
		else
			s->total += div_u64(v.disk_sectors, expected_node_disk_sectors);
	}
}

void bch2_progress_init_count(struct progress_indicator *s,
			      const char *msg,
			      enum bch_progress_units units,
			      u64 total)
{
	memset(s, 0, sizeof(*s));

	s->msg		= msg ? strip_bch2(msg) : NULL;
	s->units	= units;
	s->total	= total;
	s->next_print	= jiffies + HZ * 10;
}

static inline bool progress_update_p(struct progress_indicator *s)
{
	bool ret = time_after_eq(jiffies, s->next_print);

	if (ret)
		s->next_print = jiffies + HZ * 10;
	return ret;
}

static void progress_maybe_print(struct bch_fs *c, struct progress_indicator *s)
{
	/*
	 * Progress decides this for itself instead of leaving it to the stdio
	 * redirect: bch_info() sends log traffic to dmesg whenever the redirect
	 * is user-only, which is exactly the case where a mount is reading
	 * progress out of BCH_IOCTL_RECOVERY_STATUS and drawing it. Checked
	 * ahead of progress_update_p(), which would otherwise eat the interval
	 * we want to print on as soon as the reader goes away.
	 */
	if (READ_ONCE(c->stdio_progress_reader))
		return;

	if (s->silent || !s->msg || !progress_update_p(s))
		return;

	CLASS(printbuf, buf)();
	prt_printf(&buf, "%s ", s->msg);
	bch2_progress_to_text(&buf, s);
	bch_info(c, "%s", buf.buf);
}

int bch2_progress_update_iter(struct btree_trans *trans,
			      struct progress_indicator *s,
			      struct btree_iter *iter)
{
	struct bch_fs *c = trans->c;

	try(bch2_recovery_cancelled(c));

	struct btree *b = path_l(btree_iter_path(trans, iter))->b;

	if (IS_ERR_OR_NULL(b))
		return 0;

	struct bbpos pos = BBPOS(b->c.btree_id, b->key.k.p);

	s->seen  += b != s->last_node && bbpos_cmp(pos, s->pos) > 0;
	s->last_node	= b;
	s->pos		= pos;

	progress_maybe_print(c, s);

	return 0;
}

void bch2_progress_update_count(struct bch_fs *c, struct progress_indicator *s)
{
	s->seen++;
	progress_maybe_print(c, s);
}

__cold void bch2_progress_to_text(struct printbuf *out, struct progress_indicator *s)
{
	unsigned percent = s->total
		? div64_u64(s->seen * 100, s->total)
		: 0;
	prt_printf(out, "%d%%, done %llu/%llu %s",
		   percent, s->seen, s->total, bch2_progress_units[s->units]);

	/* No node means no position: a counter-based indicator, or nothing seen yet */
	if (!s->last_node)
		return;

	prt_str(out, ", at ");
	bch2_bbpos_to_text(out, s->pos);
}
