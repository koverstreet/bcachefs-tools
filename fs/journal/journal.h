/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_JOURNAL_H
#define _BCACHEFS_JOURNAL_H

/*
 * THE JOURNAL:
 *
 * The primary purpose of the journal is to log updates (insertions) to the
 * b-tree, to avoid having to do synchronous updates to the b-tree on disk.
 *
 * Without the journal, the b-tree is always internally consistent on
 * disk - and in fact, in the earliest incarnations bcache didn't have a journal
 * but did handle unclean shutdowns by doing all index updates synchronously
 * (with coalescing).
 *
 * Updates to interior nodes still happen synchronously and without the journal
 * (for simplicity) - this may change eventually but updates to interior nodes
 * are rare enough it's not a huge priority.
 *
 * This means the journal is relatively separate from the b-tree; it consists of
 * just a list of keys and journal replay consists of just redoing those
 * insertions in same order that they appear in the journal.
 *
 * PERSISTENCE:
 *
 * For synchronous updates (where we're waiting on the index update to hit
 * disk), the journal entry will be written out immediately (or as soon as
 * possible, if the write for the previous journal entry was still in flight).
 *
 * Synchronous updates are specified by passing a closure (@flush_cl) to
 * bch2_btree_insert() or bch_btree_insert_node(), which then pass that parameter
 * down to the journalling code. That closure will wait on the journal write to
 * complete (via closure_wait()).
 *
 * If the index update wasn't synchronous, the journal entry will be
 * written out after 10 ms have elapsed, by default (the delay_ms field
 * in struct journal).
 *
 * JOURNAL ENTRIES:
 *
 * A journal entry is variable size (struct jset), it's got a fixed length
 * header and then a variable number of struct jset_entry entries.
 *
 * Journal entries are identified by monotonically increasing 64 bit sequence
 * numbers - jset->seq; other places in the code refer to this sequence number.
 *
 * A jset_entry entry contains one or more bkeys (which is what gets inserted
 * into the b-tree). We need a container to indicate which b-tree the key is
 * for; also, the roots of the various b-trees are stored in jset_entry entries
 * (one for each b-tree) - this lets us add new b-tree types without changing
 * the on disk format.
 *
 * We also keep some things in the journal header that are logically part of the
 * superblock - all the things that are frequently updated. This is for future
 * bcache on raw flash support; the superblock (which will become another
 * journal) can't be moved or wear leveled, so it contains just enough
 * information to find the main journal, and the superblock only has to be
 * rewritten when we want to move/wear level the main journal.
 *
 * JOURNAL LAYOUT ON DISK:
 *
 * The journal is written to a ringbuffer of buckets (which is kept in the
 * superblock); the individual buckets are not necessarily contiguous on disk
 * which means that journal entries are not allowed to span buckets, but also
 * that we can resize the journal at runtime if desired (unimplemented).
 *
 * The journal buckets exist in the same pool as all the other buckets that are
 * managed by the allocator and garbage collection - garbage collection marks
 * the journal buckets as metadata buckets.
 *
 * OPEN/DIRTY JOURNAL ENTRIES:
 *
 * Open/dirty journal entries are journal entries that contain b-tree updates
 * that have not yet been written out to the b-tree on disk. We have to track
 * which journal entries are dirty, and we also have to avoid wrapping around
 * the journal and overwriting old but still dirty journal entries with new
 * journal entries.
 *
 * On disk, this is represented with the "last_seq" field of struct jset;
 * last_seq is the first sequence number that journal replay has to replay.
 *
 * To avoid overwriting dirty journal entries on disk, we keep a mapping (in
 * journal_device->seq) of for each journal bucket, the highest sequence number
 * any journal entry it contains. Then, by comparing that against last_seq we
 * can determine whether that journal bucket contains dirty journal entries or
 * not.
 *
 * To track which journal entries are dirty, we maintain a fifo of refcounts
 * (where each entry corresponds to a specific sequence number) - when a ref
 * goes to 0, that journal entry is no longer dirty.
 *
 * Journalling of index updates is done at the same time as the b-tree itself is
 * being modified (see btree_insert_key()); when we add the key to the journal
 * the pending b-tree write takes a ref on the journal entry the key was added
 * to. If a pending b-tree write would need to take refs on multiple dirty
 * journal entries, it only keeps the ref on the oldest one (since a newer
 * journal entry will still be replayed if an older entry was dirty).
 *
 * JOURNAL FILLING UP:
 *
 * There are two ways the journal could fill up; either we could run out of
 * space to write to, or we could have too many open journal entries and run out
 * of room in the fifo of refcounts. Since those refcounts are decremented
 * without any locking we can't safely resize that fifo, so we handle it the
 * same way.
 *
 * If the journal fills up, we start flushing dirty btree nodes until we can
 * allocate space for a journal write again - preferentially flushing btree
 * nodes that are pinning the oldest journal entries first.
 */

#include <linux/hash.h>

#include "journal/types_gen.h"

#include "journal/journal_gen.h"

static inline bool journal_low_on_space(struct journal *j)
{
	return test_bit(JOURNAL_low_on_space, &j->flags) ||
		test_bit(JOURNAL_low_on_pin, &j->flags);
}

/* Sequence number of oldest dirty journal entry */

static inline u64 journal_cur_seq(struct journal *j)
{
	return atomic64_read(&j->seq);
}

/*
 * Fastpath access to the staging buffer (jset) for a held reservation. No
 * lock required: the reservation holds a count on the ring slot's state
 * index, so the slot still points at the data for res->seq until the
 * reservation is released.
 */
static inline struct jset *
journal_res_data(struct journal *j, struct journal_res *res)
{
	return j->ring_data[res->seq & JOURNAL_STATE_BUF_MASK];
}

/*
 * Amount of space that will be taken up by some keys in the journal (i.e.
 * including the jset header)
 */
static inline unsigned jset_u64s(unsigned u64s)
{
	return u64s + sizeof(struct jset_entry) / sizeof(u64);
}

static inline struct jset_entry *
journal_res_entry(struct journal *j, struct journal_res *res)
{
	return vstruct_idx(journal_res_data(j, res), res->offset);
}

static inline unsigned journal_entry_init(struct jset_entry *entry, unsigned type,
					  enum btree_id id, unsigned level,
					  unsigned u64s)
{
	entry->u64s	= cpu_to_le16(u64s);
	entry->btree_id = id;
	entry->level	= level;
	entry->type	= type;
	entry->pad[0]	= 0;
	entry->pad[1]	= 0;
	entry->pad[2]	= 0;
	return jset_u64s(u64s);
}

static inline unsigned journal_entry_set(struct jset_entry *entry, unsigned type,
					  enum btree_id id, unsigned level,
					  const void *data, unsigned u64s)
{
	unsigned ret = journal_entry_init(entry, type, id, level, u64s);

	memcpy_u64s(entry->_data, data, u64s);
	return ret;
}

static inline struct jset_entry *
__bch2_journal_add_entry(struct jset_entry **cur,
			 unsigned type, enum btree_id id,
			 unsigned level, unsigned u64s)
{
	struct jset_entry *entry = *cur;
	unsigned actual = journal_entry_init(entry, type, id, level, u64s);

	*cur = (struct jset_entry *) ((u64 *) entry + actual);
	return entry;
}

static inline int bch2_journal_error(struct journal *j)
{
	return j->reservations.cur_entry_offset == JOURNAL_ENTRY_ERROR_VAL
		? -BCH_ERR_journal_shutdown : 0;
}

static inline void bch2_journal_res_flush(struct journal *j, struct journal_res *res,
					  struct closure *cl)
{
	__bch2_journal_flush_seq_async(j, res->seq, cl);
}

#endif /* _BCACHEFS_JOURNAL_H */
