/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_JOURNAL_RECLAIM_H
#define _BCACHEFS_JOURNAL_RECLAIM_H

#include "alloc/replicas.h"
#include "sb/members.h"

static inline void journal_reclaim_kick(struct journal *j)
{
	struct task_struct *p = READ_ONCE(j->reclaim_thread);

	j->reclaim_kicked = true;
	if (p)
		wake_up_process(p);
}

void bch2_journal_set_watermark(struct journal *);

void __bch2_journal_set_alloc_watermark(struct journal *, u8 *, unsigned);

/*
 * Report allocator scarcity to the journal: @watermark is the level at and
 * below which the allocator is refusing, and @p is the field it feeds
 * (j->watermark_open_buckets, ...) - see bch2_journal_set_watermark().
 *
 * Called from allocator fast paths on every bucket taken and given back, so
 * the unchanged case has to be a compare and nothing else.
 */
static inline void bch2_journal_set_alloc_watermark(struct journal *j, u8 *p,
						    unsigned watermark)
{
	if (unlikely(READ_ONCE(*p) != watermark))
		__bch2_journal_set_alloc_watermark(j, p, watermark);
}

void bch2_journal_space_available(struct journal *);

static inline bool journal_pin_active(struct journal_entry_pin *pin)
{
	return pin->seq != 0;
}

#include "journal/reclaim_gen.h"
#include "journal/pins_gen.h"

/*
 * The pin only stores the device list (the key); the refcount lives in the
 * superblock replicas table. Rebuild a replicas entry from the compact list
 * for the get/put/eq/to_text calls.
 */
static inline void journal_seq_devs_to_replicas(union bch_replicas_padded *r,
						struct journal *j, u64 seq)
{
	bch2_devlist_to_replicas(&r->e, BCH_DATA_journal, bch2_journal_seq_devs(j, seq));
}

static inline enum journal_pin_type journal_pin_type_btree(unsigned level)
{
	return JOURNAL_PIN_TYPE_btree0 - level;
}

static inline void bch2_journal_pin_add(struct journal *j, u64 seq,
					struct journal_entry_pin *pin,
					enum journal_pin_type type,
					journal_pin_flush_fn flush_fn)
{
	if (unlikely(!journal_pin_active(pin) || pin->seq > seq))
		bch2_journal_pin_set(j, seq, pin, type, flush_fn);
}

static inline void bch2_journal_pin_update(struct journal *j, u64 seq,
					   struct journal_entry_pin *pin,
					   enum journal_pin_type type,
					   journal_pin_flush_fn flush_fn)
{
	if (unlikely(!journal_pin_active(pin) || pin->seq < seq))
		bch2_journal_pin_set(j, seq, pin, type, flush_fn);
}

void bch2_journal_discard_work(struct work_struct *);
void bch2_journal_do_discards(struct journal *);
int bch2_journal_reclaim(struct journal *);

void bch2_journal_reclaim_stop(struct journal *);
int bch2_journal_reclaim_start(struct journal *);

static inline bool bch2_journal_flush_all_pins(struct journal *j)
{
	return bch2_journal_flush_pins(j, U64_MAX);
}

static inline bool bch2_journal_flush_outstanding_pins(struct journal *j)
{
	return bch2_journal_flush_pins(j, journal_cur_seq(j));
}

int bch2_journal_flush_device_pins(struct journal *, int);
void bch2_journal_flush_dev_ro(struct journal *, unsigned);

void bch2_journal_pins_to_text(struct printbuf *, struct journal *, unsigned);
void bch2_journal_reclaim_to_text(struct printbuf *, struct journal *);

#endif /* _BCACHEFS_JOURNAL_RECLAIM_H */
