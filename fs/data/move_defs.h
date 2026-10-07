/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DATA_MOVE_DEFS_H
#define _BCACHEFS_DATA_MOVE_DEFS_H

struct bch_read_bio;

/*
 * Tracks in-flight data movement IO for ratelimiting.
 *
 * Four atomic counters track sectors and IOs in flight:
 *  - read_sectors/read_ios: extent read submit -> read completion
 *  - write_sectors/write_ios: read completion -> write completion
 *
 * bch2_move_ratelimit() blocks the caller until all counters are below
 * c->opts.move_bytes_in_flight / move_ios_in_flight.
 *
 * Extent moves (bch2_move_extent) and stripe repairs (bch2_stripe_repair)
 * both account through these counters.
 *
 * Lifetime: every in-flight IO holds closure_get(&ctxt->cl).
 * bch2_moving_ctxt_flush_all() waits for all IO via closure_sync(),
 * and bch2_moving_ctxt_exit() asserts all counters are zero.
 */
struct moving_context {
	struct btree_trans	*trans;
	struct list_head	list;
	void			*fn;

	struct bch_ratelimit	*rate;
	struct bch_move_stats	*stats;
	struct write_point_specifier wp;
	bool			wait_on_copygc;

	/* For waiting on outstanding reads and writes: */
	struct closure		cl;

	struct mutex		lock;
	struct list_head	reads;
	struct list_head	ios;
	u64			io_seq;

	/* in flight sectors: */
	atomic_t		read_sectors;
	atomic_t		write_sectors;
	atomic_t		read_ios;
	atomic_t		write_ios;

	wait_queue_head_t	wait;
};

typedef int (*move_pred_fn)(struct btree_trans *, void *, enum btree_id, struct bkey_s_c,
			    struct bch_inode_opts *, struct data_update_opts *);

#endif /* _BCACHEFS_DATA_MOVE_DEFS_H */
