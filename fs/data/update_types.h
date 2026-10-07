/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DATA_UPDATE_TYPES_H
#define _BCACHEFS_DATA_UPDATE_TYPES_H

#include "enum_kind.h"

struct moving_context;

#define BCH_DATA_UPDATE_TYPES()	\
	x(other)		\
	x(copygc)		\
	x(reconcile)		\
	x(promote)		\
	x(self_heal)		\
	x(scrub)		\
	x(scrub_no_repair)

enum __enum_closed bch_data_update_types {
#define x(n)	BCH_DATA_UPDATE_##n,
	BCH_DATA_UPDATE_TYPES()
#undef x
};

/*
 * @target: where the new copy goes. Movers rewriting existing data want the
 * extent's background_target here: unset doesn't mean "no preference", it means
 * "anywhere", which drags data off its tier.
 *
 * It also decides the disk label of any erasure coded stripe the write creates
 * (bch2_ec_stripe_head_get()), and that label is stamped into the stripe on
 * disk permanently, where 0 means every device in the filesystem - so a mover
 * that leaves this unset doesn't just misplace one extent, it creates a stripe
 * that will keep widening onto devices the data was never supposed to touch.
 */
struct data_update_opts {
	enum bch_data_update_types	type;
	u8				ptrs_io_error;
	u8				ptrs_kill;
	u8				ptrs_kill_ec;
	u8				extra_replicas;
	u16				target;
	bool				no_devs_have:1;
	bool				checksum_paranoia:1;

	unsigned			read_dev;
	enum bch_read_flags		read_flags;
	enum bch_write_flags		write_flags;
	enum bch_trans_commit_flags	commit_flags;
};

struct data_update {
	struct rcu_head		rcu;
	/* extent being updated: */
	enum btree_id		btree_id;
	struct bkey_buf		k;
	struct data_update_opts	opts;

	bool			on_hashtable;
	bool			read_done;
	/*
	 * cas[i] is the bch_dev * for which we hold a ref (taken in
	 * bkey_get_dev_refs), parallel to the ptrs in @k.  Stashed so the
	 * exit path doesn't have to re-derive ca via c->devs[idx], which
	 * dev_remove may have cleared while our ref still pins the dev.
	 * NULL = no ref held for that ptr position; also serves as the
	 * "we locked this bucket" indicator for nocow lock/unlock.
	 */
	struct bch_dev		*cas[BCH_BKEY_PTRS_MAX];

	struct rhlist_head	hash;
	struct bbpos		pos;

	/* associated with @ctxt */
	struct list_head	read_list;
	struct list_head	io_list;
	u64			io_seq;
	struct move_bucket	*b;
	struct moving_context	*ctxt;
	struct bch_move_stats	*stats;

	struct bch_read_bio	rbio;
	struct bch_write_op	op;
	struct bio_vec		*bvecs;
};

struct promote_op {
	u64			start_time;
#ifdef CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS
	unsigned		list_idx;
#endif
	int			cpu; /* for promote_limit */

	struct work_struct	work;
	struct data_update	write;
	struct bio_vec		bi_inline_vecs[]; /* must be last */
};

#endif /* _BCACHEFS_DATA_UPDATE_TYPES_H */
