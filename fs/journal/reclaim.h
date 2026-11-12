/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_JOURNAL_RECLAIM_H
#define _BCACHEFS_JOURNAL_RECLAIM_H

#include "alloc/replicas.h"
#include "sb/members.h"

static inline bool journal_pin_active(struct journal_entry_pin *pin)
{
	return pin->seq != 0;
}

#include "journal/pins_gen.h"

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

#endif /* _BCACHEFS_JOURNAL_RECLAIM_H */
