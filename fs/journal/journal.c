// SPDX-License-Identifier: GPL-2.0
/*
 * bcachefs journalling code, for btree insertions
 *
 * Copyright 2012 Google, Inc.
 */

#include "bcachefs.h"

#include "alloc/foreground.h"

#include "btree/write_buffer.h"

#include "init/error.h"
#include "init/fs.h"

#include "journal/journal.h"
#include "journal/reclaim.h"
#include "journal/seq_blacklist.h"
#include "journal/write.h"

#include "sb/counters.h"

#include "util/enumerated_ref.h"

/* DOC_LATEX(journal)
 * The journal is a write-ahead log for metadata. Instead of writing every
 * \hyperref[sec:btrees]{btree} update directly to its btree node on disk,
 * bcachefs records updates in the journal first. This allows metadata writes to be batched and sequential,
 * dramatically improving performance.
 *
 * \subsubsection{How the journal works}
 *
 * Each journal entry (\texttt{struct jset}) contains a list of typed sub-entries:
 * btree key updates, btree root pointers, timestamps, IO clock values, and
 * diagnostic messages. Entries are assigned monotonically increasing sequence
 * numbers that survive crashes and are never reused.
 *
 * The journal is stored as a ring buffer of buckets on each device. As new
 * entries are written, they advance through the ring. Old entries are reclaimed
 * once all the btree nodes they refer to have been flushed to disk.
 *
 * \subsubsection{Journal pins and reclaim}
 *
 * A \emph{journal pin} holds a reference from a dirty btree node (or key cache
 * entry) to the journal sequence that contains its latest update. Journal space
 * for a sequence cannot be reclaimed until all pins referencing that sequence
 * are released---which happens when the corresponding btree node is written to
 * disk.
 *
 * The journal reclaim thread runs in the background, identifying which btree
 * nodes are pinning the oldest journal sequences and flushing them. Under normal
 * operation this is invisible; under heavy write load, reclaim may need to work
 * harder to keep up.
 *
 * \subsubsection{Space pressure}
 *
 * When free journal space drops below 25\%, or the pin list fills to 75\%
 * capacity, the journal enters a reclaim watermark state. In this state:
 *
 * \begin{itemize}
 * \item New metadata writes may be throttled
 * \item The reclaim thread is woken to aggressively flush btree nodes
 * \item Space is typically freed within milliseconds as nodes flush
 * \end{itemize}
 *
 * If the journal fills completely, metadata operations block until space is
 * freed. This is rare under normal workloads and resolves automatically. A
 * sustained ``journal full'' condition typically indicates that btree node
 * writes are bottlenecked---often by a slow device or high IO contention.
 *
 * \subsubsection{Flush and ordering}
 *
 * Journal writes come in two flavors:
 *
 * \begin{description}
 * \item[Flush writes] Ordered to stable storage with disk cache flushes. These
 *   provide durability guarantees---data acknowledged to applications via
 *   \texttt{fsync()} is protected by flush writes. A configurable delay
 *   (\texttt{journal\_flush\_delay}, default 1000\,ms) batches updates before
 *   flushing.
 * \item[No-flush writes] Written without ordering guarantees. These can be lost
 *   on power failure but are much cheaper. Used between flush points to reduce
 *   IO overhead.
 * \end{description}
 *
 * On multi-device filesystems, flush writes issue a preflush to all devices
 * first, ensuring all pending data writes are ordered before the journal entry.
 *
 * \subsubsection{Mount and recovery}
 *
 * On mount, the journal is read from all devices. The recovery window is
 * determined by two sequence numbers: \texttt{last\_seq} (the oldest entry still
 * needed) and the sequence of the last valid flush entry. All entries in this
 * window are replayed in order, re-inserting their btree keys into the btree.
 * Journal replay is idempotent---replaying the same entry twice is safe.
 *
 * On clean shutdown, a special \texttt{clean} field is written to the
 * \hyperref[sec:superblock]{superblock} containing the btree roots and usage
 * counters, allowing the next mount to skip journal replay entirely.
 *
 * \textbf{Sequence blacklisting}: After an unclean shutdown, some btree nodes on
 * disk may reference journal sequences that were never durably committed. These
 * sequences are added to a blacklist stored in the superblock; any btree node
 * data referencing a blacklisted sequence is ignored during recovery. Once the
 * affected nodes are rewritten with new sequences, the blacklist entries are
 * garbage collected.
 *
 * \subsubsection{User-facing options}
 *
 * \begin{description}
 * \item[\texttt{journal\_flush\_delay}] Milliseconds before auto-committing the
 *   journal (default 1000). Lower values reduce the window of data loss on
 *   crash; higher values improve throughput.
 * \item[\texttt{journal\_flush\_disabled}] Disable journal flushes entirely.
 *   \textbf{Dangerous}---data loss is expected on any unclean shutdown.
 * \item[\texttt{journal\_reclaim\_delay}] Milliseconds before triggering
 *   background reclaim (default 100).
 * \item[\texttt{journal\_transaction\_names}] Log function names in journal
 *   entries for debugging (default enabled).
 * \end{description}
 *
 * Journal size is configured per device and can be resized online with
 * \texttt{bcachefs device resize-journal}.
 *
 * \subsubsection{Consistency and self-healing}
 *
 * Every journal entry is checksummed. Entries that fail checksum validation are
 * skipped during replay, with the filesystem falling back to the last known good
 * entry. The sequence blacklist mechanism ensures that partially-written state
 * from crashes cannot corrupt the btree. Journal entries are replicated across
 * devices according to the \texttt{metadata\_replicas} setting; if one device's
 * journal is unreadable, recovery proceeds from the other copies.
 */


static bool __journal_entry_is_open(union journal_res_state state)
{
	return state.cur_entry_offset < JOURNAL_ENTRY_CLOSED_VAL;
}

static bool journal_entry_is_open(struct journal *j)
{
	return __journal_entry_is_open(j->reservations);
}

static __cold void bch2_journal_buf_to_text(struct printbuf *out, struct journal *j,
				     struct journal_buf *buf, u64 seq)
{
	union journal_res_state s = READ_ONCE(j->reservations);

	prt_printf(out, "seq:\t%llu\n", seq);
	guard(printbuf_indent)(out);

	if (!buf->write_started)
		prt_printf(out, "refcount:\t%u\n", journal_state_count(s, seq & JOURNAL_STATE_BUF_MASK));

	struct closure *cl = &buf->io;
	int r = atomic_read(&cl->remaining);
	prt_printf(out, "io:\t%pS r %i\n", cl->fn, r & CLOSURE_REMAINING_MASK);

	if (buf->data) {
		prt_printf(out, "size:\t");
		prt_human_readable_u64(out, vstruct_bytes(buf->data));
		prt_newline(out);
	}

	prt_printf(out, "waiters:\t%u\n", buf->wait.list.first != NULL);
	prt_printf(out, "flags:\t");
	if (buf->separate_flush)
		prt_str(out, "separate_flush ");
	if (buf->need_flush_to_write_buffer)
		prt_str(out, "need_flush_to_write_buffer ");
	if (buf->write_started)
		prt_str(out, "write_started ");
	if (buf->write_allocated)
		prt_str(out, "write_allocated ");
	if (buf->write_done)
		prt_str(out, "write_done");
	prt_newline(out);
}

static __cold void bch2_journal_bufs_to_text(struct printbuf *out, struct journal *j)
{
	lockdep_assert_held(&j->lock);
	guard(printbuf_atomic)(out);

	if (!out->nr_tabstops)
		printbuf_tabstop_push(out, 24);

	struct journal_buf *buf;
	u64 seq;
	fifo_for_each_entry_ptr(buf, &j->in_flight, seq)
		bch2_journal_buf_to_text(out, j, buf, seq);
	prt_printf(out, "last buf %s\n", journal_entry_is_open(j) ? "open" : "closed");
}

/*
 * Detect stuck journal conditions and trigger shutdown. Technically the journal
 * can end up stuck for a variety of reasons, such as a blocked I/O, journal
 * reservation lockup, etc. Since this is a fatal error with potentially
 * unpredictable characteristics, we want to be fairly conservative before we
 * decide to shut things down.
 *
 * Consider the journal stuck when it appears full with no ability to commit
 * btree transactions, to discard journal buckets, nor acquire priority
 * (reserved watermark) reservation.
 */
static inline bool
journal_error_check_stuck(struct journal *j, int error, unsigned flags)
{
	struct bch_fs *c = container_of(j, struct bch_fs, journal);

	if (!(error == -BCH_ERR_journal_full ||
	      error == -BCH_ERR_journal_pin_full) ||
	    fifo_used(&j->in_flight) ||
	    (flags & BCH_WATERMARK_MASK) != BCH_WATERMARK_reclaim)
		return false;

	if (j->can_discard)
		return false;

	CLASS(bch_log_msg, msg)(c);
	msg.m.suppress = true; /* only print once, when we go ERO */

	prt_printf(&msg.m, "Journal stuck! Have a pre-reservation but journal full (error %s)",
		   bch2_err_str(error));
	bch2_journal_debug_to_text(&msg.m, j);
	if (test_bit(JOURNAL_low_on_wb, &j->flags))
		bch2_btree_write_buffer_to_text(&msg.m, c);

	prt_printf(&msg.m, "Journal pins:\n");
	bch2_journal_pins_to_text(&msg.m, j);

	bch2_fs_emergency_read_only(c, &msg.m);
	return true;
}

/*
 * Final processing when the last reference of a journal buffer has been
 * dropped. Drop the pin list reference acquired at journal entry open and write
 * the buffer, if requested.
 */
void __bch2_journal_buf_put_final(struct journal *j, u64 seq)
{
	lockdep_assert_held(&j->pin_resize_lock);
	lockdep_assert_held(&j->lock);

	if (__bch2_journal_pin_put(j, seq))
		bch2_journal_update_last_seq(j);
	bch2_journal_do_writes_locked(j);

	/*
	 * Wake both j->wait (for __bch2_next_write_buffer_flush_journal_buf(),
	 * when quiescing an open journal entry) and j->async_wait (for
	 * journal_res_get_slowpath() waiters blocked on journal_max_open — the
	 * refcount drop may have enabled do_writes_locked() to advance
	 * seq_write_started above).
	 */
	journal_wake(j);
}

void bch2_journal_buf_put_final(struct journal *j, u64 seq)
{
	guard(percpu_read)(&j->pin_resize_lock);
	guard(spinlock)(&j->lock);
	__bch2_journal_buf_put_final(j, seq);
}

/*
 * Returns true if journal entry is now closed:
 *
 * We don't close a journal_buf until the next journal_buf is finished writing,
 * and can be opened again - this also initializes the next journal_buf:
 */
/*
 * Close the currently-open journal entry. No recursion — callers that want
 * the close-then-maybe-open behavior of the prior __journal_entry_close go
 * through bch2_journal_cycle() instead.
 */
static void __journal_entry_close_one(struct journal *j, unsigned closed_val, bool trace)
{
	struct bch_fs *c = container_of(j, struct bch_fs, journal);
	struct journal_buf *buf = journal_cur_buf(j);
	union journal_res_state old, new;
	unsigned sectors;

	BUG_ON(closed_val != JOURNAL_ENTRY_CLOSED_VAL &&
	       closed_val != JOURNAL_ENTRY_ERROR_VAL);

	if (closed_val != JOURNAL_ENTRY_ERROR_VAL)
		lockdep_assert_held(&j->pin_resize_lock);
	lockdep_assert_held(&j->lock);

	old.v = atomic64_read(&j->reservations.counter);
	do {
		new.v = old.v;
		new.cur_entry_offset = closed_val;

		if (old.cur_entry_offset == JOURNAL_ENTRY_ERROR_VAL ||
		    old.cur_entry_offset == new.cur_entry_offset)
			return;
	} while (!atomic64_try_cmpxchg(&j->reservations.counter,
				       &old.v, new.v));

	if (!__journal_entry_is_open(old))
		return;

	u64 seq = journal_cur_seq(j);

	if (old.cur_entry_offset == JOURNAL_ENTRY_BLOCKED_VAL)
		old.cur_entry_offset = j->cur_entry_offset_if_blocked;

	/* Close out old buffer: */
	buf->data->u64s		= cpu_to_le32(old.cur_entry_offset);

	if (closed_val != JOURNAL_ENTRY_ERROR_VAL) {
		size_t bytes = roundup_pow_of_two(vstruct_bytes(buf->data));

		journal_seq_pin(j, seq)->bytes = bytes;
		j->dirty_entry_bytes += bytes;
	}

	if (trace)
		event_trace(c, journal_entry_close, msg, ({
			guard(printbuf_atomic)(&msg);

			prt_str(&msg, "entry size: ");
			prt_human_readable_u64(&msg, vstruct_bytes(buf->data));
			prt_newline(&msg);
			bch2_prt_task_backtrace(&msg, current, 1, GFP_NOWAIT);
		}));

	sectors = vstruct_blocks_plus(buf->data, c->block_bits,
				      buf->u64s_reserved) << c->block_bits;
	if (unlikely(sectors > buf->sectors)) {
		CLASS(bch_log_msg_atomic, msg)(c);
		prt_printf(&msg.m, "journal entry overran reserved space: %u > %u\n",
			   sectors, buf->sectors);
		prt_printf(&msg.m, "buf u64s %u u64s reserved %u cur_entry_u64s %u block_bits %u\n",
			   le32_to_cpu(buf->data->u64s), buf->u64s_reserved,
			   j->cur_entry_u64s,
			   c->block_bits);
		bch2_fs_emergency_read_only_locked(c, &msg.m);
		return;
	}

	buf->sectors = sectors;

	/*
	 * We have to set last_seq here, _before_ opening a new journal entry:
	 *
	 * A threads may replace an old pin with a new pin on their current
	 * journal reservation - the expectation being that the journal will
	 * contain either what the old pin protected or what the new pin
	 * protects.
	 *
	 * After the old pin is dropped j->last_seq won't include the old
	 * pin, so we can only write the updated last_seq on the entry that
	 * contains whatever the new pin protects.
	 *
	 * Restated, we can _not_ update last_seq for a given entry if there
	 * could be a newer entry open with reservations/pins that have been
	 * taken against it.
	 *
	 * Hence, we want update/set last_seq on the current journal entry right
	 * before we open a new one:
	 */
	buf->last_seq		= j->last_seq;
	buf->data->last_seq	= cpu_to_le64(buf->last_seq);
	BUG_ON(buf->last_seq > le64_to_cpu(buf->data->seq));

	cancel_delayed_work(&j->write_work);

	/*
	 * bch2_journal_halt may or may not be called with pin_resize_lock held,
	 * and we don't have a recursive version of this lock.
	 *
	 * So, on emergency ERO skip the journal_buf_put - this leaks a ref, but
	 * it's for updating last_seq, so it's ok:
	 */
	if (closed_val != JOURNAL_ENTRY_ERROR_VAL) {
		__bch2_journal_buf_put(j, seq);
		bch2_journal_space_available(j);
	} else {
		unsigned idx = seq & JOURNAL_STATE_BUF_MASK;
		union journal_res_state s = journal_state_buf_put(j, idx);
		if (!journal_state_count(s, idx))
			bch2_journal_do_writes_locked(j);
	}
}

/*
 * Open a new journal entry. No recursion — the post-open "must close
 * immediately" check lives in bch2_journal_cycle() now. Callers go through
 * bch2_journal_cycle(j, true).
 */
static int __journal_entry_open_one(struct journal *j)
{
	struct bch_fs *c = container_of(j, struct bch_fs, journal);
	union journal_res_state old, new;

	lockdep_assert_held(&j->pin_resize_lock);
	lockdep_assert_held(&j->lock);

	if (journal_entry_is_open(j))
		return 0;

	if (j->blocked)
		return bch_err_throw(c, journal_blocked);

	if (j->cur_entry_error)
		return j->cur_entry_error;

	try(bch2_journal_error(j));

	if (!fifo_free(&j->pin))
		return bch_err_throw(c, journal_pin_full);

	/*
	 * More than half full — kick off a fifo resize on the workqueue so
	 * we grow capacity before we'd start throttling. By the time we hit
	 * the low_on_pin watermark (3/4 full) the throttle has already
	 * fired, so trigger earlier. queue_work is idempotent — subsequent
	 * triggers while a resize is pending coalesce. Off the open path
	 * because some callers hit it with btree locks held
	 * (JOURNAL_RES_GET_NONBLOCK) and can't tolerate the alloc/sleep.
	 */
	if (fifo_free(&j->pin) < j->pin.size / 2)
		queue_work(j->wq, &j->pin_resize_work);

	/*
	 * Need room in the in_flight FIFO and a pre-allocated data buffer;
	 * the data buffer is topped up by journal_buf_prealloc() outside of
	 * j->lock.
	 */
	if (!fifo_free(&j->in_flight))
		return bch_err_throw(c, journal_max_in_flight);

	if (atomic64_read(&j->seq) - j->seq_write_started == JOURNAL_STATE_BUF_NR)
		return bch_err_throw(c, journal_max_open);

	if (unlikely(journal_cur_seq(j) >= JOURNAL_SEQ_MAX)) {
		CLASS(bch_log_msg_atomic, msg)(c);
		prt_printf(&msg.m, "cannot start: journal seq overflow");
		bch2_fs_emergency_read_only_locked(c, &msg.m);
		return bch_err_throw(c, journal_shutdown);
	}

	if (unlikely(bch2_journal_seq_is_blacklisted(c, journal_cur_seq(j) + 1, false))) {
		CLASS(bch_log_msg_atomic, msg)(c);
		prt_printf(&msg.m, "attempting to open blacklisted journal seq %llu",
			   journal_cur_seq(j));
		bch2_fs_emergency_read_only_locked(c, &msg.m);
		return bch_err_throw(c, journal_shutdown);
	}

	if (!j->free_buf)
		return bch_err_throw(c, journal_buf_enomem);

	BUG_ON(!j->cur_entry_sectors);

	unsigned sectors = min(j->cur_entry_sectors, j->free_buf_size >> 9);

	int u64s = (int) (sectors << 9) / sizeof(u64) - journal_entry_overhead(j);
	u64s = clamp_t(int, u64s, 0, JOURNAL_ENTRY_CLOSED_VAL - 1);

	if (u64s <= (ssize_t) j->early_journal_entries.nr)
		return bch_err_throw(c, journal_full);

	bool was_empty = fifo_empty(&j->pin);

	/*
	 * The fifo_push() needs to happen at the same time as j->seq is
	 * incremented for j->last_seq to be calculated correctly
	 */
	u64 seq = atomic64_inc_return(&j->seq);
	journal_pin_list_init(fifo_push_ref(&j->pin), 1);

	BUG_ON(j->pin.back - 1 != seq);

	/*
	 * Grab the next in_flight FIFO slot (inline storage — the pointer
	 * is stable because the FIFO is statically sized and never
	 * reallocated).
	 */
	struct journal_buf *buf = fifo_push_ref(&j->in_flight);
	memset(buf, 0, offsetof(struct journal_buf, wait));

	/* Claim the pre-allocated data buffer */
	swap(buf->data,		j->free_buf);
	swap(buf->buf_size,	j->free_buf_size);

	buf->u64s_reserved	= j->entry_u64s_reserved;
	buf->disk_sectors	= j->cur_entry_sectors;
	buf->sectors		= sectors;
	buf->j			= j;

	bkey_extent_init(&buf->key);
	/* if filesystem is clean, the first journal write must be a flush */
	if (c->sb.clean)
		set_bit(JOURNAL_need_flush_write, &j->flags);

	buf->need_flush_to_write_buffer = true;
	buf->has_overwrites	= READ_ONCE(c->opts.journal_transaction_names);

	memset(buf->data, 0, sizeof(*buf->data));
	buf->data->seq	= cpu_to_le64(seq);
	buf->data->u64s	= 0;

	BUG_ON(xchg(&buf->wait.list.first,
		    xchg(&j->flush_wait.list.first, NULL)) != JOURNAL_BUF_NOFLUSH);

	/*
	 * Publish to the reservation fastpath ring slot. Must happen before
	 * the atomic reservation-state cmpxchg below so that reservations on
	 * the new state idx see the ring slot populated.
	 */
	j->ring[seq & JOURNAL_STATE_BUF_MASK].buf  = buf;
	j->ring[seq & JOURNAL_STATE_BUF_MASK].data = buf->data;

	if (j->early_journal_entries.nr) {
		memcpy(buf->data->_data, j->early_journal_entries.data,
		       j->early_journal_entries.nr * sizeof(u64));
		le32_add_cpu(&buf->data->u64s, j->early_journal_entries.nr);
	}

	/*
	 * Must be set before marking the journal entry as open:
	 */
	j->cur_entry_u64s = u64s;

	old.v = atomic64_read(&j->reservations.counter);
	do {
		new.v = old.v;

		BUG_ON(old.cur_entry_offset == JOURNAL_ENTRY_ERROR_VAL);

		new.idx++;
		BUG_ON(journal_state_count(new, new.idx));
		BUG_ON(new.idx != (seq & JOURNAL_STATE_BUF_MASK));

		journal_state_inc(&new);

		/* Handle any already added entries */
		new.cur_entry_offset = le32_to_cpu(buf->data->u64s);
	} while (!atomic64_try_cmpxchg(&j->reservations.counter,
				       &old.v, new.v));

	journal_wake(j);

	if (was_empty && j->reclaim_thread)
		wake_up_process(j->reclaim_thread);

	if (j->early_journal_entries.nr)
		darray_exit(&j->early_journal_entries);

	return 0;
}

static inline bool journal_has_flush_waiters(struct journal *j)
{
	return j->flush_wait.list.first != NULL ||
		(journal_entry_is_open(j) &&
		 journal_buf_must_flush(journal_cur_buf(j)));
}

/*
 * Cycle the journal entry state between closed and open as needed.
 *
 * This replaces the prior mutually-recursive __journal_entry_close ↔
 * journal_entry_open. The two transitions have natural "and now maybe
 * do the other" tails:
 *   - close: a flush_wait waiter wants the new entry to absorb it
 *   - open:  the just-opened entry must flush immediately (must_flush)
 * Folding both tails into a loop here makes the state machine explicit.
 *
 * @want_open: true if the caller will reserve in the new entry afterwards
 *             (so an open is mandatory); false to just close, with open
 *             happening only if a flush_wait waiter demands it.
 *
 * Returns: error from open if open was attempted and failed, else 0.
 */
int bch2_journal_cycle(struct journal *j, bool need_fresh)
{
	lockdep_assert_held(&j->pin_resize_lock);
	lockdep_assert_held(&j->lock);

	while (1) {
		if (!need_fresh &&
		    !journal_has_flush_waiters(j) &&
		    fifo_used(&j->in_flight) > 1)
			return 0;

		__journal_entry_close_one(j, JOURNAL_ENTRY_CLOSED_VAL, true);

		if (!need_fresh &&
		    !journal_has_flush_waiters(j))
			return 0;

		try(__journal_entry_open_one(j));

		need_fresh = false;
	}
}

void bch2_journal_halt_locked(struct journal *j)
{
	lockdep_assert_held(&j->lock);

	__journal_entry_close_one(j, JOURNAL_ENTRY_ERROR_VAL, true);
	if (!j->err_seq)
		j->err_seq = journal_cur_seq(j);
	journal_wake(j);

	/*
	 * Wake closure waiters who'd otherwise be parked indefinitely once
	 * we enter error state: no future write_done will fire, so the
	 * usual per-write wake-ups (and reclaim-side wake-ups) won't reach
	 * them. They re-check on wake, see the journal is shutting down,
	 * and return the appropriate error.
	 */
	__closure_wake_up(&j->flush_wait);
	__closure_wake_up(&j->reclaim_flush_wait);
}

void bch2_journal_halt(struct journal *j)
{
	guard(spinlock)(&j->lock);
	bch2_journal_halt_locked(j);
}

void bch2_journal_entry_close(struct journal *j)
{
	guard(percpu_read)(&j->pin_resize_lock);
	guard(spinlock)(&j->lock);
	bch2_journal_cycle(j, false);
}

static bool journal_quiesced(struct journal *j)
{
	guard(percpu_read)(&j->pin_resize_lock);
	guard(spinlock)(&j->lock);
	bool ret = atomic64_read(&j->seq) == j->seq_ondisk;

	if (!ret)
		bch2_journal_cycle(j, false);
	return ret;
}

void bch2_journal_quiesce(struct journal *j)
{
	wait_event(j->wait, journal_quiesced(j));
}

/*
 * Shutdown-only quiesce: bch2_journal_quiesce() waits for writes to have
 * landed on disk, but journal_write_done() still has bookkeeping to do after
 * the write completes - notably bch2_journal_update_last_seq_ondisk() and
 * replicas_refs_put(), which is what triggers the final mark_clean via
 * __replicas_entry_kill() + bch2_write_super(). Wait for all of that to
 * drain so the fs ends up marked clean.
 *
 * On journal error, flushed_seq_ondisk stops advancing (the update path in
 * journal_write_done is gated on !j->err_seq), so fall back to the regular
 * quiesce condition (seq == seq_ondisk), which always terminates since
 * seq_ondisk is updated unconditionally for every completed write. Handles
 * the race where the journal transitions to error state mid-wait.
 */
static bool journal_shutdown_quiesced(struct journal *j)
{
	guard(percpu_read)(&j->pin_resize_lock);
	guard(spinlock)(&j->lock);
	u64 seq = atomic64_read(&j->seq);
	bool ret = (bch2_journal_error(j)
		    ? seq == j->seq_ondisk
		    : seq == j->flushed_seq_ondisk) &&
		!j->flush_wait.list.first;

	if (!ret)
		bch2_journal_cycle(j, false);
	return ret;
}

void bch2_journal_shutdown_quiesce(struct journal *j)
{
	wait_event(j->wait, journal_shutdown_quiesced(j));
}

/*
 * The journal auto-commit timer: re-armed for journal_flush_delay whenever a
 * flush write is submitted (in bch2_journal_do_writes_locked()), so it only
 * actually fires once that long has passed with no commit - at which point we
 * close the open entry to get it written out.
 */
void bch2_journal_write_work(struct work_struct *work)
{
	struct journal *j = container_of(work, struct journal, write_work.work);

	guard(percpu_read)(&j->pin_resize_lock);
	guard(spinlock)(&j->lock);
	bch2_journal_cycle(j, true);
}

static void journal_buf_prealloc(struct journal *j)
{
	if (j->free_buf &&
	    j->free_buf_size >= j->buf_size_want)
		return;

	unsigned buf_size = j->buf_size_want;

	spin_unlock(&j->lock);
	void *buf = kvmalloc(buf_size, GFP_NOFS);
	spin_lock(&j->lock);

	if (!buf)
		return;

	swap(buf,	j->free_buf);
	swap(buf_size,	j->free_buf_size);

	if (unlikely(buf)) {
		spin_unlock(&j->lock);
		/* kvfree can sleep */
		kvfree(buf);
		spin_lock(&j->lock);
	}
}

static int __journal_res_get(struct journal *j, struct journal_res *res,
			     unsigned flags)
{
	struct bch_fs *c = container_of(j, struct bch_fs, journal);
	struct journal_buf *buf;
	int ret;
retry:
	if (journal_res_get_fast(j, res, flags))
		return 0;

	ret = bch2_journal_error(j);
	if (unlikely(ret))
		return ret;

	if (j->blocked)
		return bch_err_throw(c, journal_blocked);

	if ((flags & BCH_WATERMARK_MASK) < j->watermark) {
		ret = bch_err_throw(c, journal_full);
		goto out;
	}

	percpu_down_read(&j->pin_resize_lock);
	spin_lock(&j->lock);

	journal_buf_prealloc(j);

	/*
	 * Recheck after taking the lock, so we don't race with another thread
	 * that just did journal_entry_open() and call bch2_journal_entry_close()
	 * unnecessarily
	 */
	if (journal_res_get_fast(j, res, flags)) {
		ret = 0;
		goto unlock;
	}

	/*
	 * If we couldn't get a reservation because the current buf filled up,
	 * and we had room for a bigger entry on disk, signal that we want to
	 * realloc the journal bufs:
	 */
	buf = journal_cur_buf(j);
	if (journal_entry_is_open(j) &&
	    buf->buf_size >> 9 < buf->disk_sectors &&
	    buf->buf_size < JOURNAL_ENTRY_SIZE_MAX)
		j->buf_size_want = max(j->buf_size_want, buf->buf_size << 1);

	ret = bch2_journal_cycle(j, true) ?: -BCH_ERR_journal_retry_open;
unlock:
	spin_unlock(&j->lock);
	percpu_up_read(&j->pin_resize_lock);
out:
	if (likely(!ret))
		return 0;
	if (ret == -BCH_ERR_journal_retry_open)
		goto retry;

	if (journal_error_check_stuck(j, ret, flags))
		ret = bch_err_throw(c, journal_stuck);

	if (ret == -BCH_ERR_journal_blocked)
		track_event_change(&c->times[BCH_TIME_blocked_journal_write_buffer_flush], true);

	if (ret == -BCH_ERR_journal_max_in_flight)
		track_event_change(&c->times[BCH_TIME_blocked_journal_max_in_flight], true);

	if (ret == -BCH_ERR_journal_max_open)
		track_event_change(&c->times[BCH_TIME_blocked_journal_max_open], true);

	if (bch2_err_matches(ret, BCH_ERR_operation_blocked))
		event_inc_trace(c, journal_res_get_blocked, buf, ({
			prt_printf(&buf, "%s\n", bch2_err_str(ret));
			bch2_printbuf_make_room(&buf, 4096);

			scoped_guard(spinlock, &j->lock) {
				prt_printf(&buf, "seq %llu\n", journal_cur_seq(j));
				bch2_journal_bufs_to_text(&buf, j);
			}
		}));

	/*
	 * Journal is full - can't rely on reclaim from work item due to
	 * freezing:
	 */
	if ((ret == -BCH_ERR_journal_full ||
	     ret == -BCH_ERR_journal_pin_full) &&
	    !(flags & JOURNAL_RES_GET_NONBLOCK)) {
		if (journal_low_on_space(j) &&
		    mutex_trylock(&j->reclaim_lock)) {
			bch2_journal_reclaim(j);
			mutex_unlock(&j->reclaim_lock);
		}
	}

	return ret;
}

static unsigned max_dev_latency(struct bch_fs *c)
{
	u64 nsecs = 0;

	guard(rcu)();
	for_each_member_device_rcu(c, ca, &c->allocator.rw_devs[BCH_DATA_journal])
		nsecs = max(nsecs, ca->io_latency[WRITE].stats.max_duration);

	return nsecs_to_jiffies(nsecs);
}

/*
 * Essentially the entry function to the journaling code. When bcachefs is doing
 * a btree insert, it calls this function to get the current journal write.
 * Journal write is the structure used set up journal writes. The calling
 * function will then add its keys to the structure, queuing them for the next
 * write.
 *
 * To ensure forward progress, the current task must not be holding any
 * btree node write locks.
 */
int bch2_journal_res_get_slowpath(struct journal *j, struct journal_res *res,
				  unsigned flags,
				  struct btree_trans *trans)
{
	if (flags & JOURNAL_RES_GET_NONBLOCK)
		return __journal_res_get(j, res, flags);

	struct bch_fs *c = container_of(j, struct bch_fs, journal);
	long total_wait = max(max_dev_latency(c) * 2, HZ * 10);
	int ret;

	if (trans_wait_event_timeout(trans, &j->async_wait,
		   !bch2_err_matches(ret = __journal_res_get(j, res, flags), BCH_ERR_operation_blocked) ||
		   (flags & JOURNAL_RES_GET_NONBLOCK),
		   total_wait))
		return ret;

	CLASS(printbuf, buf)();
	prt_printf(&buf, bch2_fmt(c, "Journal stuck? Waited for 10 seconds, err %s"), bch2_err_str(ret));
	bch2_journal_debug_to_text(&buf, j);
	if (test_bit(JOURNAL_low_on_wb, &j->flags))
		bch2_btree_write_buffer_to_text(&buf, c);
	bch2_print_str(c, KERN_ERR, buf.buf);

	trans_wait_event(trans, &j->async_wait,
		   !bch2_err_matches(ret = __journal_res_get(j, res, flags), BCH_ERR_operation_blocked) ||
		   (flags & JOURNAL_RES_GET_NONBLOCK));
	return ret;
}

/* journal_entry_res: */

void bch2_journal_entry_res_resize(struct journal *j,
				   struct journal_entry_res *res,
				   unsigned new_u64s)
{
	union journal_res_state state;
	int d = new_u64s - res->u64s;

	guard(percpu_read)(&j->pin_resize_lock);
	guard(spinlock)(&j->lock);

	j->entry_u64s_reserved	+= d;
	res->u64s		+= d;

	if (d <= 0)
		return;

	j->cur_entry_u64s = max_t(int, 0, j->cur_entry_u64s - d);
	state = READ_ONCE(j->reservations);

	if (state.cur_entry_offset >= JOURNAL_ENTRY_CLOSED_VAL)
		return;

	if (state.cur_entry_offset > j->cur_entry_u64s) {
		j->cur_entry_u64s += d;
		/*
		 * Not enough room in current journal entry, have to flush it:
		 */
		bch2_journal_cycle(j, false);
	} else {
		journal_cur_buf(j)->u64s_reserved += d;
	}
}

/* journal flushing: */

/*
 * Variant of closure_wait() that respects the JOURNAL_BUF_NOFLUSH sentinel:
 * if the journal_buf has been committed to being submitted noflush, no
 * waiters can attach and we return false. Otherwise the closure is attached
 * to buf->wait and returns true; the act of attaching transitions buf out
 * of the "may be noflush" state into "must flush" (via the wait-list head
 * becoming a real pointer).
 *
 * Returns true if the closure was attached, false if poisoned or @cl is
 * already on a waitlist.
 */
static bool journal_buf_wait(struct journal_buf *buf, struct closure *cl)
{
	/* Optimistic check: bail without touching closure state if visibly poisoned */
	if (READ_ONCE(buf->wait.list.first) == JOURNAL_BUF_NOFLUSH)
		return false;

	/* Commit to waiting */
	closure_set_waiting(cl, _RET_IP_);
	unsigned r = atomic_add_return(CLOSURE_WAITING + 1, &cl->remaining);
	closure_val_checks(cl, r, CLOSURE_WAITING + 1);

	/*
	 * cmpxchg-loop attach, bailing on race-to-poison. Standard llist_add
	 * would happily chain through the NOFLUSH sentinel, which would
	 * corrupt the list — so we open-code the attach to check first each
	 * iteration.
	 */
	struct llist_node *first = READ_ONCE(buf->wait.list.first);
	do {
		if (first == JOURNAL_BUF_NOFLUSH) {
			/* Poisoned after we committed waiting state — roll back.
			 * Safe because cl isn't on any list yet, so no concurrent
			 * writer can touch cl->remaining. */
			atomic_sub(CLOSURE_WAITING + 1, &cl->remaining);
			return false;
		}
		cl->list.next = first;
	} while (!try_cmpxchg(&buf->wait.list.first, &first, &cl->list));

	return true;
}

void __bch2_journal_flush_seq_async(struct journal *j, u64 seq, struct closure *cl)
{
	BUG_ON(atomic_read(&cl->remaining) & CLOSURE_WAITING);

	bool found = false;

	for (; seq <= journal_cur_seq(j); seq++)
		if (journal_buf_wait(&fifo_entry(&j->in_flight, seq), cl)) {
			found = true;
			break;
		}

	if (!found)
		BUG_ON(!closure_wait(&j->flush_wait, cl));

	if (journal_entry_is_open(j)
	    ? fifo_used(&j->in_flight) == 1
	    : j->flush_wait.list.first != NULL)
		bch2_journal_entry_close(j);
}

/**
 * bch2_journal_flush_seq_async - wait for a journal entry to be written
 * @j:		journal object
 * @seq:	seq to flush
 * @parent:	closure object to wait with
 * Returns:	1 if @seq has already been flushed, 0 if @seq is being flushed,
 *		-BCH_ERR_journal_flush_err if @seq will never be flushed
 *
 * Like bch2_journal_wait_on_seq, except that it triggers a write immediately if
 * necessary
 */
int bch2_journal_flush_seq_async(struct journal *j, u64 seq, struct closure *cl)
{
	struct bch_fs *c = container_of(j, struct bch_fs, journal);

	BUG_ON(atomic_read(&cl->remaining) & CLOSURE_WAITING);

	if (seq <= j->flushed_seq_ondisk)
		return 1;

	WARN_ONCE(seq > journal_cur_seq(j),
		  "requested to flush journal seq %llu, but currently at %llu",
		  seq, journal_cur_seq(j));

	/*
	 * In error state, any seq that wasn't already durable before the
	 * error fired won't ever flush — write_done's flushed_seq_ondisk
	 * update is gated on !err_seq, so seqs in flight at the moment of
	 * error are stranded with seq_ondisk advanced but
	 * flushed_seq_ondisk not. Return -EIO for those instead of
	 * waiting forever; fsync gets the error and unblocks.
	 */
	if (j->err_seq && seq > j->flushed_seq_ondisk)
		return bch_err_throw(c, journal_flush_err);

	__bch2_journal_flush_seq_async(j, seq, cl);
	return 0;
}

int bch2_journal_flush_seq(struct journal *j, u64 seq, unsigned task_state)
{
	struct bch_fs *c = container_of(j, struct bch_fs, journal);
	u64 start_time = local_clock();

	CLASS(closure_stack, cl)();
	int ret = bch2_journal_flush_seq_async(j, seq, &cl);

	if (closure_sync_timeout(&cl, HZ * 10)) {
		CLASS(printbuf, buf)();
		prt_printf(&buf, bch2_fmt(c,
			"bch2_journal_flush_seq stuck? Waited 10s for seq %llu"), seq);
		bch2_journal_debug_to_text(&buf, j);
		bch2_print_str(c, KERN_ERR, buf.buf);
		closure_sync(&cl);
	}

	/* Don't update time_stats when @seq is already flushed: */
	if (!ret)
		bch2_time_stats_update(j->flush_seq_time, start_time);

	return j->err_seq && seq >= j->err_seq
		? bch_err_throw(c, journal_flush_err)
		: 0;
}

static CLOSURE_CALLBACK(closure_free)
{
	struct closure *cl = container_of(ws, struct closure, work);
	kfree(cl);
}

/*
 * bch2_journal_flush_async - if there is an open journal entry, or a journal
 * still being written, write it and wait for the write to complete
 */
void bch2_journal_flush_async(struct journal *j, struct closure *cl)
{
	if (!cl) {
		cl = kmalloc(sizeof(*cl), GFP_KERNEL);
		closure_init(cl, NULL);
		bch2_journal_flush_seq_async(j, atomic64_read(&j->seq), cl);
		continue_at(cl, closure_free, NULL);
	} else {
		bch2_journal_flush_seq_async(j, atomic64_read(&j->seq), cl);
	}
}

int bch2_journal_flush(struct journal *j)
{
	return bch2_journal_flush_seq(j, atomic64_read(&j->seq), TASK_UNINTERRUPTIBLE);
}

/*
 * bch2_journal_noflush_seq - ask the journal not to issue any flushes in the
 * range [start, end)
 * @seq
 */
bool bch2_journal_noflush_seq(struct journal *j, u64 start, u64 end)
{
	struct bch_fs *c = container_of(j, struct bch_fs, journal);

	if (!(c->sb.features & (1ULL << BCH_FEATURE_journal_no_flush)))
		return false;

	if (c->journal.flushed_seq_ondisk >= start)
		return false;

	for (u64 seq = start; seq < end; seq++) {
		struct journal_buf *buf = &fifo_entry(&j->in_flight, seq);
		if (cmpxchg(&buf->wait.list.first, NULL, JOURNAL_BUF_NOFLUSH) != NULL)
			return false;
	}

	return true;
}

/*
 * Advance the rewind limit so that discards up to @seq become safe.
 * Must be called before bch2_journal_flush() to persist the new limit.
 */
void bch2_journal_advance_rewind_seq(struct journal *j, u64 seq)
{
	scoped_guard(spinlock, &j->lock)
		j->rewind_seq = max(j->rewind_seq, seq);
}

int bch2_journal_add_rewind_range(struct bch_fs *c, u64 from, u64 to)
{
	struct journal *j = &c->journal;

	struct journal_rewind_range range = {
		.from	= from,
		.to	= to,
	};
	try(darray_push(&j->rewind_ranges, range));

	unsigned u64s = 2;
	try(darray_make_room(&j->early_journal_entries, jset_u64s(u64s)));
	struct jset_entry_rewind *rw =
		(void *) &darray_top(j->early_journal_entries);
	journal_entry_init(&rw->entry, BCH_JSET_ENTRY_rewind, 0, 0, u64s);
	rw->from	= cpu_to_le64(from);
	rw->to		= cpu_to_le64(to);
	j->early_journal_entries.nr += jset_u64s(u64s);

	return 0;
}

int __bch2_journal_meta(struct journal *j)
{
	CLASS(closure_stack, cl)();

	struct journal_res res = {};
	try(bch2_journal_res_get(j, &res, jset_u64s(0), 0, NULL));
	bch2_journal_res_flush(j, &res, &cl);
	bch2_journal_res_put(j, &res);

	closure_sync(&cl);

	return bch2_journal_error(j);
}

int bch2_journal_meta(struct journal *j)
{
	struct bch_fs *c = container_of(j, struct bch_fs, journal);

	if (!enumerated_ref_tryget(&c->writes, BCH_WRITE_REF_journal))
		return bch_err_throw(c, erofs_no_writes);

	int ret = __bch2_journal_meta(j);
	enumerated_ref_put(&c->writes, BCH_WRITE_REF_journal);
	return ret;
}

/* block/unlock the journal: */

void bch2_journal_unblock(struct journal *j)
{
	scoped_guard(spinlock, &j->lock)
		if (!--j->blocked &&
		    j->cur_entry_offset_if_blocked < JOURNAL_ENTRY_CLOSED_VAL &&
		    j->reservations.cur_entry_offset == JOURNAL_ENTRY_BLOCKED_VAL) {
			union journal_res_state old, new;

			old.v = atomic64_read(&j->reservations.counter);
			do {
				new.v = old.v;
				new.cur_entry_offset = j->cur_entry_offset_if_blocked;
			} while (!atomic64_try_cmpxchg(&j->reservations.counter, &old.v, new.v));

			struct bch_fs *c = container_of(j, struct bch_fs, journal);
			track_event_change(&c->times[BCH_TIME_blocked_journal_write_buffer_flush], false);
		}

	journal_wake(j);
}

static void __bch2_journal_block(struct journal *j)
{
	if (!j->blocked++) {
		union journal_res_state old, new;

		old.v = atomic64_read(&j->reservations.counter);
		do {
			j->cur_entry_offset_if_blocked = old.cur_entry_offset;

			if (j->cur_entry_offset_if_blocked >= JOURNAL_ENTRY_CLOSED_VAL)
				break;

			new.v = old.v;
			new.cur_entry_offset = JOURNAL_ENTRY_BLOCKED_VAL;
		} while (!atomic64_try_cmpxchg(&j->reservations.counter, &old.v, new.v));

		if (old.cur_entry_offset < JOURNAL_ENTRY_BLOCKED_VAL)
			journal_cur_buf(j)->data->u64s = cpu_to_le32(old.cur_entry_offset);
	}
}

void bch2_journal_block(struct journal *j)
{
	scoped_guard(spinlock, &j->lock)
		__bch2_journal_block(j);

	bch2_journal_quiesce(j);
}

static struct journal_buf *__bch2_next_write_buffer_flush_journal_buf(struct journal *j,
						u64 max_seq, bool *blocked)
{
	struct journal_buf *ret = NULL;

	/* We're inside wait_event(), but using mutex_lock(: */
	sched_annotate_sleep();
	mutex_lock(&j->buf_lock);
	guard(spinlock)(&j->lock);
	max_seq = min(max_seq, journal_cur_seq(j));

	struct journal_buf *buf;
	u64 seq;
	fifo_for_each_entry_ptr(buf, &j->in_flight, seq) {
		if (seq > max_seq)
			break;

		if (buf->need_flush_to_write_buffer) {
			union journal_res_state s;
			s.v = atomic64_read_acquire(&j->reservations.counter);

			unsigned open = seq == journal_cur_seq(j) && __journal_entry_is_open(s);

			if (open && !*blocked) {
				__bch2_journal_block(j);
				s.v = atomic64_read_acquire(&j->reservations.counter);
				*blocked = true;
			}

			ret = journal_state_seq_count(j, s, seq) > open
				? ERR_PTR(-EAGAIN)
				: buf;
			break;
		}
	}

	if (IS_ERR_OR_NULL(ret))
		mutex_unlock(&j->buf_lock);
	return ret;
}

struct journal_buf *bch2_next_write_buffer_flush_journal_buf(struct journal *j,
							     u64 max_seq, bool *blocked)
{
	struct journal_buf *ret;
	*blocked = false;

	wait_event(j->wait, (ret = __bch2_next_write_buffer_flush_journal_buf(j,
						max_seq, blocked)) != ERR_PTR(-EAGAIN));
	if (IS_ERR_OR_NULL(ret) && *blocked)
		bch2_journal_unblock(j);

	return ret;
}

/* debug: */

static const char * const bch2_journal_flags_strs[] = {
#define x(n)	#n,
	JOURNAL_FLAGS()
#undef x
	NULL
};

__cold void __bch2_journal_debug_to_text(struct printbuf *out, struct journal *j)
{
	struct bch_fs *c = container_of(j, struct bch_fs, journal);
	union journal_res_state s;
	unsigned long now = jiffies;
	u64 nr_writes = j->nr_flush_writes + j->nr_noflush_writes;

	printbuf_tabstops_reset(out);
	printbuf_tabstop_push(out, 28);

	guard(printbuf_atomic)(out);
	guard(rcu)();

	s = READ_ONCE(j->reservations);

	prt_printf(out, "flags:\t");
	prt_bitflags(out, bch2_journal_flags_strs, j->flags);
	prt_newline(out);
	prt_printf(out, "dirty journal entries:\t%llu/%llu\n",	fifo_used(&j->pin), j->pin.size);
	prt_printf(out, "seq:\t%llu\n",				journal_cur_seq(j));
	prt_printf(out, "seq_ondisk:\t%llu\n",			j->seq_ondisk);
	prt_printf(out, "last_seq:\t%llu\n",			j->last_seq);
	prt_printf(out, "last_seq_ondisk:\t%llu\n",		j->last_seq_ondisk);
	prt_printf(out, "flushed_seq_ondisk:\t%llu\n",		j->flushed_seq_ondisk);
	prt_printf(out, "last_empty_seq:\t%llu\n",		j->last_empty_seq);
	prt_printf(out, "rewind_seq:\t%llu\n",			j->rewind_seq);
	prt_printf(out, "rewind_seq_ondisk:\t%llu\n",		j->rewind_seq_ondisk);
	prt_printf(out, "watermark:\t%s\n",			bch2_watermarks[j->watermark]);
	prt_printf(out, "each entry reserved:\t%u\n",		j->entry_u64s_reserved);
	prt_printf(out, "nr flush writes:\t%llu\n",		j->nr_flush_writes);
	prt_printf(out, "nr noflush writes:\t%llu\n",		j->nr_noflush_writes);
	prt_printf(out, "average write size:\t");
	prt_human_readable_u64(out, nr_writes ? div64_u64(j->entry_bytes_written, nr_writes) : 0);
	prt_newline(out);
	prt_printf(out, "free buf:\t%u\n",			j->free_buf ? j->free_buf_size : 0);
	prt_printf(out, "nr direct reclaim:\t%llu\n",		j->nr_direct_reclaim);
	prt_printf(out, "nr background reclaim:\t%llu\n",	j->nr_background_reclaim);
	prt_printf(out, "reclaim kicked:\t%u\n",		j->reclaim_kicked);
	prt_printf(out, "reclaim runs in:\t%u ms\n",		time_after(j->next_reclaim, now)
	       ? jiffies_to_msecs(j->next_reclaim - jiffies) : 0);
	prt_printf(out, "blocked:\t%u\n",			j->blocked);
	prt_printf(out, "flush_wait:\t%u\n",			j->flush_wait.list.first != NULL);
	prt_printf(out, "current entry sectors:\t%u\n",		j->cur_entry_sectors);
	prt_printf(out, "current entry error:\t%s\n",		bch2_err_str(j->cur_entry_error));
	prt_printf(out, "current entry:\t");

	switch (s.cur_entry_offset) {
	case JOURNAL_ENTRY_ERROR_VAL:
		prt_printf(out, "error\n");
		break;
	case JOURNAL_ENTRY_CLOSED_VAL:
		prt_printf(out, "closed\n");
		break;
	case JOURNAL_ENTRY_BLOCKED_VAL:
		prt_printf(out, "blocked\n");
		break;
	default:
		prt_printf(out, "%u/%u\n", s.cur_entry_offset, j->cur_entry_u64s);
		break;
	}

	prt_printf(out, "unwritten entries:\n");
	bch2_journal_bufs_to_text(out, j);

	prt_printf(out, "space:\n");
	scoped_guard(printbuf_indent, out) {
		prt_printf(out, "discarded\t%u:%u\n",
		       j->space[journal_space_discarded].next_entry,
		       j->space[journal_space_discarded].total);
		prt_printf(out, "clean ondisk\t%u:%u\n",
		       j->space[journal_space_clean_ondisk].next_entry,
		       j->space[journal_space_clean_ondisk].total);
		prt_printf(out, "clean\t%u:%u\n",
		       j->space[journal_space_clean].next_entry,
		       j->space[journal_space_clean].total);
		prt_printf(out, "total\t%u:%u\n",
		       j->space[journal_space_total].next_entry,
		       j->space[journal_space_total].total);
	}

	for_each_member_device_rcu(c, ca, &c->allocator.rw_devs[BCH_DATA_journal]) {
		struct journal_device *ja = &ca->journal;
		if (!ja->nr)
			continue;

		prt_printf(out, "dev %u:\n",			ca->dev_idx);
		prt_printf(out, "durability %u:\n",		ca->mi.durability);
		guard(printbuf_indent)(out);
		prt_printf(out, "nr\t%u\n",			ja->nr);
		prt_printf(out, "bucket size\t%u\n",		ca->mi.bucket_size);
		prt_printf(out, "available\t%u:%u\n",		bch2_journal_dev_buckets_available(j, ja, journal_space_discarded), ja->sectors_free);
		prt_printf(out, "discard_idx\t%u\n",		ja->discard_idx);
		prt_printf(out, "dirty_ondisk\t%u (seq %llu)\n",ja->dirty_idx_ondisk,	ja->bucket_seq[ja->dirty_idx_ondisk]);
		prt_printf(out, "dirty_idx\t%u (seq %llu)\n",	ja->dirty_idx,		ja->bucket_seq[ja->dirty_idx]);
		prt_printf(out, "cur_idx\t%u (seq %llu)\n",	ja->cur_idx,		ja->bucket_seq[ja->cur_idx]);
	}

	prt_printf(out, "replicas %u\n", c->opts.metadata_replicas);
}

__cold void bch2_journal_debug_to_text(struct printbuf *out, struct journal *j)
{
	guard(spinlock)(&j->lock);
	__bch2_journal_debug_to_text(out, j);
}
