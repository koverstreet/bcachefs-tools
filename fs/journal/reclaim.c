// SPDX-License-Identifier: GPL-2.0

#include "bcachefs.h"

#include "alloc/buckets.h"
#include "alloc/replicas.h"

#include "btree/key_cache.h"
#include "btree/update.h"
#include "btree/write_buffer.h"

#include "init/error.h"

#include "journal/journal.h"
#include "journal/reclaim.h"

#include "sb/counters.h"
#include "sb/members.h"

#include <linux/kthread.h>
#include <linux/sched/mm.h>

#define JOURNAL_PIN_DEADLOCK_SKIP_MAX	128

static bool __should_discard_bucket(struct journal *j, struct journal_device *ja)
{
	unsigned min_free = max(4, ja->nr / 2);

	return bch2_journal_dev_buckets_available(j, ja, journal_space_discarded) <
		min_free &&
		ja->discard_idx != ja->dirty_idx_ondisk;
}

/* Free space calculations: */

static unsigned journal_space_from(struct journal_device *ja,
				   enum journal_space_from from)
{
	switch (from) {
	case journal_space_discarded:
		return ja->discard_idx;
	case journal_space_clean_ondisk:
		return ja->dirty_idx_ondisk;
	case journal_space_clean:
		return ja->dirty_idx;
	default:
		BUG();
	}
}

unsigned bch2_journal_dev_buckets_available(struct journal *j,
					    struct journal_device *ja,
					    enum journal_space_from from)
{
	if (!ja->nr)
		return 0;

	unsigned available = (journal_space_from(ja, from) -
			      ja->cur_idx - 1 + ja->nr) % ja->nr;

	/*
	 * Don't use the last bucket unless writing the new last_seq
	 * will make another bucket available:
	 */
	if (available && ja->dirty_idx_ondisk == ja->dirty_idx)
		--available;

	return available;
}

void bch2_journal_set_watermark(struct journal *j)
{
	struct bch_fs *c = container_of(j, struct bch_fs, journal);

	bool med_on_space = (j->space[journal_space_clean].total * 4 <=
			     j->space[journal_space_total].total * 3);

	bool low_on_space = j->space[journal_space_clean].total * 4 <=
		j->space[journal_space_total].total;
	bool low_on_pin = fifo_free(&j->pin) < j->pin.size / 4;
	bool low_on_wb = bch2_btree_write_buffer_must_wait(c);

	unsigned watermark = low_on_space || low_on_pin || low_on_wb
		? BCH_WATERMARK_reclaim
		: BCH_WATERMARK_stripe;

	if (track_event_change(&c->times[BCH_TIME_blocked_journal_low_on_space], low_on_space) |
	    track_event_change(&c->times[BCH_TIME_blocked_journal_low_on_pin], low_on_pin) |
	    track_event_change(&c->times[BCH_TIME_blocked_write_buffer_full], low_on_wb))
		event_inc_trace(c, journal_full, buf, ({
			guard(printbuf_atomic)(&buf);
			prt_printf(&buf, "low_on_space %u\n",	low_on_space);
			prt_printf(&buf, "low_on_pin %u\n",	low_on_pin);
			prt_printf(&buf, "low_on_wb %u\n",	low_on_wb);
			if (low_on_wb)
				bch2_btree_write_buffer_to_text(&buf, c);
		}));

	mod_bit(JOURNAL_med_on_space,	&j->flags, med_on_space);

	mod_bit(JOURNAL_low_on_space,	&j->flags, low_on_space);
	mod_bit(JOURNAL_low_on_pin,	&j->flags, low_on_pin);
	mod_bit(JOURNAL_low_on_wb,	&j->flags, low_on_wb);

	swap(watermark, j->watermark);
	if (watermark > j->watermark)
		journal_wake(j);
	if (med_on_space)
		journal_reclaim_kick(j);
}

static struct journal_space
journal_dev_space_available(struct journal *j, struct bch_dev *ca,
			    enum journal_space_from from)
{
	struct bch_fs *c = container_of(j, struct bch_fs, journal);
	struct journal_device *ja = &ca->journal;
	unsigned bucket_size_aligned = round_down(ca->mi.bucket_size, block_sectors(c));

	/*
	 * Both .total values are capped by the in-memory dirty budget: we
	 * can't keep more than RAM/4 of journal dirty in memory, so the space
	 * the watermark sees is bounded by RAM, not by raw device capacity.
	 * (A global budget, applied per-device; the nr_devs_want-th largest
	 * pick in __journal_space_available() keeps the aggregate bounded.)
	 *
	 * The cap is asymmetric, and that asymmetry is the point: the watermark
	 * ratio (clean * 4 <= total, see bch2_journal_set_watermark()) has to
	 * measure how full the dirty budget is, not how big the disk is. So:
	 *
	 *  - journal_space_total is capped at a flat RAM/4 - dirty is NOT
	 *    subtracted, so it's a constant ceiling.
	 *  - journal_space_clean/_clean_ondisk are capped at RAM/4 - dirty,
	 *    so they shrink as dirty grows, and the ratio falls as the budget
	 *    fills. That's the intended soft throttle.
	 *
	 * Both halves matter. Leaving journal_space_total at true device
	 * capacity (as it once was) breaks it the other way: any fs whose
	 * journal is larger than RAM has clean permanently small vs total, so
	 * low_on_space is stuck on even when the journal is nearly empty.
	 * Conversely, subtracting dirty from journal_space_total too would let
	 * total shrink toward 0 as dirty grows and strangle the journal. A flat
	 * RAM/4 ceiling for total is what keeps both failure modes away.
	 *
	 * Only .total is clamped, not next_entry: .total feeds the watermark
	 * (a soft throttle - low-priority writers wait, reclaim-priority writes
	 * still proceed), whereas next_entry feeds cur_entry_sectors, the hard
	 * reservation limit. Clamping next_entry would let the budget drive
	 * cur_entry_sectors to 0, blocking even the journal writes that advance
	 * last_seq and let dirty drain - a self-deadlock. The RAM budget must
	 * stay a soft limit.
	 */
	size_t mem_limit = totalram_pages() * PAGE_SIZE / 4;

	if (from == journal_space_total)
		return (struct journal_space) {
			.next_entry	= bucket_size_aligned,
			.total		= min(bucket_size_aligned * ja->nr,
					      mem_limit >> 9),
		};

	unsigned buckets = bch2_journal_dev_buckets_available(j, ja, from);
	unsigned sectors = round_down(ja->sectors_free, block_sectors(c));

	/*
	 * We that we don't allocate the space for a journal entry
	 * until we write it out - thus, account for it here:
	 */
	struct journal_buf *buf;
	u64 seq;
	fifo_for_each_entry_ptr(buf, &j->in_flight, seq) {
		unsigned unwritten = buf->sectors;
		if (!unwritten)
			continue;

		/* entry won't fit on this device, skip: */
		if (unwritten > bucket_size_aligned)
			continue;

		if (unwritten >= sectors) {
			if (!buckets) {
				sectors = 0;
				break;
			}

			buckets--;
			sectors = bucket_size_aligned;
		}

		sectors -= unwritten;
	}

	if (sectors < ca->mi.bucket_size && buckets) {
		buckets--;
		sectors = bucket_size_aligned;
	}

	mem_limit = max_t(ssize_t, 0, mem_limit - j->dirty_entry_bytes);

	return (struct journal_space) {
		.next_entry	= sectors,
		.total		= min(sectors + buckets * bucket_size_aligned, mem_limit >> 9),
	};
}

static struct journal_space __journal_space_available(struct journal *j, unsigned nr_devs_want,
			    enum journal_space_from from)
{
	struct bch_fs *c = container_of(j, struct bch_fs, journal);
	unsigned pos, nr_devs = 0;
	struct journal_space space, dev_space[BCH_REPLICAS_MAX];
	unsigned min_bucket_size = U32_MAX;

	BUG_ON(nr_devs_want > ARRAY_SIZE(dev_space));

	for_each_member_device_rcu(c, ca, &c->allocator.rw_devs[BCH_DATA_journal]) {
		if (!ca->journal.nr)
			continue;

		min_bucket_size = min(min_bucket_size, ca->mi.bucket_size);

		space = journal_dev_space_available(j, ca, from);
		if (!space.next_entry)
			continue;

		/*
		 * Only the @nr_devs_want largest matter: top-k insertion
		 * sort, largest to smallest, entries past nr_devs_want fall
		 * off. nr_devs still counts every qualifying device.
		 */
		unsigned nr_kept = min(nr_devs, nr_devs_want);

		for (pos = 0; pos < nr_kept; pos++)
			if (space.total > dev_space[pos].total)
				break;

		if (pos < nr_devs_want) {
			memmove(&dev_space[pos + 1],
				&dev_space[pos],
				(min(nr_kept, nr_devs_want - 1) - pos) * sizeof(dev_space[0]));
			dev_space[pos] = space;
		}
		nr_devs++;
	}

	if (nr_devs < nr_devs_want)
		return (struct journal_space) { 0, 0 };

	/*
	 * It's possible for bucket size to be misaligned w.r.t. the filesystem
	 * block size:
	 */
	min_bucket_size = round_down(min_bucket_size, block_sectors(c));

	/*
	 * We sorted largest to smallest, and we want the smallest out of the
	 * @nr_devs_want largest devices:
	 */
	space = dev_space[nr_devs_want - 1];
	space.next_entry = min(space.next_entry, min_bucket_size);
	return space;
}

void bch2_journal_space_available(struct journal *j)
{
	struct bch_fs *c = container_of(j, struct bch_fs, journal);
	unsigned clean, clean_ondisk, total;

	/*
	 * Max entry size we'll let callers use — take the min over any
	 * currently-live ring buffers, or fall back to the target buf size
	 * when no ring slot has a buf assigned yet.
	 */
	unsigned max_entry_size = UINT_MAX;
	struct journal_buf *buf;
	u64 seq;
	fifo_for_each_entry_ptr(buf, &j->in_flight, seq)
		if (buf->buf_size)
			max_entry_size = min(max_entry_size, buf->buf_size >> 9);
	if (max_entry_size == UINT_MAX)
		max_entry_size = j->buf_size_want >> 9;

	unsigned nr_online = 0, nr_devs_want;
	bool can_discard = false;

	lockdep_assert_held(&j->lock);
	guard(rcu)();

	for_each_member_device_rcu(c, ca, &c->allocator.rw_devs[BCH_DATA_journal]) {
		struct journal_device *ja = &ca->journal;

		if (!ja->nr)
			continue;

		while (ja->dirty_idx != ja->cur_idx &&
		       ja->bucket_seq[ja->dirty_idx] < j->last_seq)
			ja->dirty_idx = (ja->dirty_idx + 1) % ja->nr;

		while (ja->dirty_idx_ondisk != ja->dirty_idx &&
		       ja->bucket_seq[ja->dirty_idx_ondisk] < j->last_seq_ondisk)
			ja->dirty_idx_ondisk = (ja->dirty_idx_ondisk + 1) % ja->nr;

		max_entry_size = min_t(unsigned, max_entry_size, ca->mi.bucket_size);
		nr_online++;

		can_discard |= __should_discard_bucket(j, ja);

		if (__should_discard_bucket(j, ja) &&
		    test_bit(BCH_FS_rw_init_done, &c->flags) &&
		    test_bit(JOURNAL_running, &j->flags) &&
		    !bch2_journal_error(j))
			queue_work(j->discard_wq, &ja->discard);
	}

	j->can_discard = can_discard;

	if (!nr_online) {
		if (!(c->sb.features & BIT_ULL(BCH_FEATURE_small_image))) {
			CLASS(printbuf, buf)();
			guard(printbuf_atomic)(&buf);
			prt_printf(&buf, "no writeable journal devices available\n");

			for_each_member_device_rcu(c, ca, &c->allocator.rw_devs[BCH_DATA_journal])
				prt_printf(&buf, " %s", ca->name);

			bch_err(c, "%s", buf.buf);
		}

		j->cur_entry_sectors	= 0;
		j->cur_entry_error	= bch_err_throw(c, insufficient_journal_devices);
		return;
	}

	nr_devs_want = min_t(unsigned, nr_online, c->opts.metadata_replicas);

	for (unsigned i = 0; i < journal_space_nr; i++)
		j->space[i] = __journal_space_available(j, nr_devs_want, i);

	clean_ondisk	= j->space[journal_space_clean_ondisk].total;
	clean		= j->space[journal_space_clean].total;
	total		= j->space[journal_space_total].total;

	if ((j->space[journal_space_clean_ondisk].next_entry <
	     j->space[journal_space_clean_ondisk].total) &&
	    (clean - clean_ondisk <= total / 8) &&
	    (clean_ondisk * 2 > clean))
		set_bit(JOURNAL_may_skip_flush, &j->flags);
	else
		clear_bit(JOURNAL_may_skip_flush, &j->flags);

	bch2_journal_set_watermark(j);

	j->cur_entry_sectors	= j->space[journal_space_discarded].next_entry;
	j->cur_entry_error	= j->cur_entry_sectors
		? 0
		: bch_err_throw(c, journal_full);

	if (!j->cur_entry_error)
		journal_wake(j);
}

/* Discards - last part of journal reclaim: */

static bool should_discard_bucket(struct journal *j, struct journal_device *ja)
{
	guard(spinlock)(&j->lock);
	return __should_discard_bucket(j, ja);
}

static void bch2_journal_dev_do_discards(struct journal_device *ja)
{
	struct bch_dev *ca = container_of(ja, struct bch_dev, journal);
	struct bch_fs *c = ca->fs;
	struct journal *j = &c->journal;

	if (!bch2_dev_get_ioref(c, ca->dev_idx, WRITE, BCH_DEV_WRITE_REF_journal_discard))
		return;

	guard(mutex)(&ja->discard_lock);

	while (should_discard_bucket(j, ja)) {
		if (!c->opts.nochanges &&
		    bch2_discard_opt_enabled(c, ca) &&
		    bdev_max_discard_sectors(ca->disk_sb.bdev))
			blkdev_issue_discard(ca->disk_sb.bdev,
					     bucket_to_sector(ca,
							      ja->buckets[ja->discard_idx]),
					     ca->mi.bucket_size, GFP_NOIO);

		scoped_guard(spinlock, &j->lock) {
			ja->discard_idx = (ja->discard_idx + 1) % ja->nr;
			bch2_journal_space_available(j);
		}
	}

	enumerated_ref_put(&ca->io_ref[WRITE],
			   BCH_DEV_WRITE_REF_journal_discard);
}

/*
 * Advance ja->discard_idx as long as it points to buckets that are no longer
 * dirty, issuing discards if necessary:
 */
void bch2_journal_discard_work(struct work_struct *work)
{
	struct journal_device *ja = container_of(work, struct journal_device, discard);

	bch2_journal_dev_do_discards(ja);
}

void bch2_journal_do_discards(struct journal *j)
{
	struct bch_fs *c = container_of(j, struct bch_fs, journal);

	for_each_member_device(c, ca)
		bch2_journal_dev_do_discards(&ca->journal);
}

/*
 * Journal entry pinning - machinery for holding a reference on a given journal
 * entry, holding it open to ensure it gets replayed during recovery:
 */

void bch2_journal_update_last_seq(struct journal *j)
{
	lockdep_assert_held(&j->lock);

	/*
	 * Unpin journal entries whose reference counts reached zero, meaning
	 * all btree nodes got written out
	 */
	u64 old = j->last_seq;
	struct journal_entry_pin_list *pin_list;
	while (j->last_seq <  j->pin.back &&
	       j->last_seq <= j->seq_ondisk &&
	       !atomic_read(&(pin_list = journal_seq_pin(j, j->last_seq))->count))
		j->last_seq++;

	if (old != j->last_seq) {
		bch2_journal_space_available(j);
		__closure_wake_up(&j->reclaim_flush_wait);
	}
}

static void bch2_journal_maybe_update_last_seq(struct journal *j)
{
	lockdep_assert_held(&j->pin_resize_lock);

	if (!atomic_read(&fifo_entry(&j->pin, j->last_seq).count)) {
		guard(spinlock)(&j->lock);
		bch2_journal_update_last_seq(j);
	}
}

int bch2_journal_update_last_seq_ondisk(struct journal *j, u64 last_seq_ondisk,
					darray_replicas_entry_refs *refs)
{
	BUG_ON(last_seq_ondisk > j->pin.back);

	for (u64 seq = j->last_seq_ondisk; seq < last_seq_ondisk; seq++) {
		struct journal_entry_pin_list *pin_list = journal_seq_pin(j, seq);

		if (pin_list->devs.nr) {
			union bch_replicas_padded devs_r;
			journal_pin_devs_to_replicas(&devs_r, pin_list);

			replicas_entry_refs *e = darray_find_p(*refs, i,
			    bch2_replicas_entry_eq(&i->replicas.e, &devs_r.e));

			if (e) {
				e->nr_refs++;
			} else {
				try(darray_push_gfp(refs, ((replicas_entry_refs) {
						    .nr_refs = 1,
						    .replicas = devs_r,
				}), GFP_ATOMIC));
			}

			pin_list->devs.nr = 0;
		}

		if (WARN_ON(j->dirty_entry_bytes < pin_list->bytes))
			pin_list->bytes = j->dirty_entry_bytes;

		j->dirty_entry_bytes -= pin_list->bytes;
		pin_list->bytes = 0;
	}

	return 0;
}

void bch2_journal_replay_pins_put(struct journal *j, u64 seq)
{
	BUG_ON(seq < j->replay_journal_seq);

	seq = min(seq, j->replay_journal_seq_end);

	guard(percpu_read)(&j->pin_resize_lock);

	while (j->replay_journal_seq < seq) {
		struct journal_entry_pin_list *pin_list =
			journal_seq_pin(j, j->replay_journal_seq++);

		BUG_ON(!pin_list->unreplayed);
		pin_list->unreplayed = false;

		if (atomic_dec_and_test(&pin_list->count)) {
			guard(spinlock)(&j->lock);
			bch2_journal_update_last_seq(j);
		}
	}
}

static inline bool journal_pin_drop_locked(struct journal *j,
					   struct journal_entry_pin_list *pin_l,
					   struct journal_entry_pin *pin)
{
	if (!journal_pin_active(pin))
		return false;

	if (j->flush_in_progress == pin)
		j->flush_in_progress_dropped = true;

	list_del_init(&pin->list);
	/*
	 * Don't clear pin->seq here; caller may be updating the pin, and we
	 * don't want other threads to see pin->seq in an intermediate state
	 */

	if (j->reclaim_flush_wait.list.first)
		__closure_wake_up(&j->reclaim_flush_wait);

	/*
	 * Unpinning a journal entry may make journal_next_bucket() succeed, if
	 * writing a new last_seq will now make another bucket available:
	 */
	return atomic_dec_and_test(&pin_l->count) && pin_l== journal_seq_pin(j, j->last_seq);
}

void bch2_journal_pin_drop(struct journal *j,
			   struct journal_entry_pin *pin)
{
	bool reclaim = false;

	guard(percpu_read)(&j->pin_resize_lock);

	while (true) {
		u64 seq = READ_ONCE(pin->seq);
		if (!seq)
			break;

		struct journal_entry_pin_list *pin_l = &fifo_entry(&j->pin, seq);
		guard(spinlock)(&pin_l->lock);
		if (pin->seq != seq)
			continue;

		reclaim = journal_pin_drop_locked(j, pin_l, pin);
		pin->seq = 0;
		break;
	}

	if (reclaim)
		bch2_journal_maybe_update_last_seq(j);
}

static enum journal_pin_type journal_pin_type(struct journal_entry_pin *pin,
					      journal_pin_flush_fn fn)
{
	if (fn == bch2_btree_node_flush0 ||
	    fn == bch2_btree_node_flush1) {
		unsigned idx = fn == bch2_btree_node_flush1;
		struct btree *b = container_of(pin, struct btree, writes[idx].journal);

		return JOURNAL_PIN_TYPE_btree0 - b->c.level;
	} else if (fn == bch2_btree_key_cache_journal_flush)
		return JOURNAL_PIN_TYPE_key_cache;
	else
		return JOURNAL_PIN_TYPE_other;
}

static bool journal_pin_is_skipped(struct journal_entry_pin *pin,
				   struct journal_entry_pin **skipped,
				   unsigned nr_skipped)
{
	for (unsigned i = 0; i < nr_skipped; i++)
		if (skipped[i] == pin)
			return true;
	return false;
}

static inline bool bch2_journal_pin_set_locked(struct journal *j,
			struct journal_entry_pin_list *old_l,
			struct journal_entry_pin_list *new_l,
			struct journal_entry_pin *pin,
			u64 seq, journal_pin_flush_fn flush_fn)
{
	bool reclaim = journal_pin_active(pin)
		? journal_pin_drop_locked(j, old_l, pin)
		: false;

	/*
	 * flush_fn is how we identify journal pins in debugfs, so must always
	 * exist, even if it doesn't do anything:
	 */
	BUG_ON(!flush_fn);

	atomic_inc(&new_l->count);
	pin->seq	= seq;
	pin->flush	= flush_fn;

	enum journal_pin_type type = journal_pin_type(pin, flush_fn);

	if (list_empty(&new_l->unflushed[type]) &&
	    j->reclaim_flush_wait.list.first)
		__closure_wake_up(&j->reclaim_flush_wait);

	list_add(&pin->list, &new_l->unflushed[type]);

	return reclaim;
}

static struct journal_entry_pin_list *maybe_seq_pin(struct journal *j, u64 seq)
{
	return seq ? &fifo_entry(&j->pin, seq) : NULL;
}

void bch2_journal_pin_copy(struct journal *j,
			   struct journal_entry_pin *dst,
			   struct journal_entry_pin *src,
			   journal_pin_flush_fn flush_fn)
{
	guard(percpu_read)(&j->pin_resize_lock);

	while (true) {
		u64 src_seq = READ_ONCE(src->seq);
		u64 dst_seq = READ_ONCE(dst->seq);

		if (!src_seq)
			break;

		struct journal_entry_pin_list *src_l = maybe_seq_pin(j, src_seq);
		struct journal_entry_pin_list *dst_l = maybe_seq_pin(j, dst_seq);

		if (!dst_l || dst_l == src_l) {
			spin_lock(&src_l->lock);
		} else if (dst_seq < src_seq) {
			spin_lock(&dst_l->lock);
			spin_lock_nested(&src_l->lock, 1);
		} else {
			spin_lock(&src_l->lock);
			spin_lock_nested(&dst_l->lock, 1);
		}

		bool reclaim = false, race = src_seq != src->seq || dst_seq != dst->seq;
		if (!race)
			reclaim = bch2_journal_pin_set_locked(j, dst_l, src_l, dst, src_seq, flush_fn);

		if (dst_l && dst_l != src_l)
			spin_unlock(&dst_l->lock);
		spin_unlock(&src_l->lock);

		if (!race) {
			/*
			 * If the journal is currently full,  we might want to call flush_fn
			 * immediately:
			 */
			if (src_seq == j->last_seq)
				journal_wake(j);
			if (reclaim)
				bch2_journal_maybe_update_last_seq(j);
			break;
		}
	}
}

void bch2_journal_pin_set(struct journal *j, u64 new_seq,
			  struct journal_entry_pin *pin,
			  journal_pin_flush_fn flush_fn)
{
	guard(percpu_read)(&j->pin_resize_lock);

	/*
	 * fifo_entry() below masks without a range check, so a pin outside the
	 * live window lands on an out-of-[front,back) slot: that pin is never
	 * reclaimed (reclaim only walks the window) and is left dangling in the
	 * old buffer when the fifo is resized+freed. Catch the offending caller
	 * here rather than debugging the eventual use-after-free.
	 */
	WARN_ONCE(new_seq < j->pin.front || new_seq >= j->pin.back,
		  "journal pin set for seq %llu outside live range [%llu, %llu)",
		  new_seq, j->pin.front, j->pin.back);

	while (true) {
		u64 old_seq = READ_ONCE(pin->seq);

		struct journal_entry_pin_list *new_l = &fifo_entry(&j->pin, new_seq);
		struct journal_entry_pin_list *old_l = maybe_seq_pin(j, old_seq);

		if (!old_l || old_l == new_l) {
			spin_lock(&new_l->lock);
		} else if (old_seq < new_seq) {
			spin_lock(&old_l->lock);
			spin_lock_nested(&new_l->lock, 1);
		} else {
			spin_lock(&new_l->lock);
			spin_lock_nested(&old_l->lock, 1);
		}

		bool reclaim = false, race = old_seq != pin->seq;
		if (!race)
			reclaim = bch2_journal_pin_set_locked(j, old_l, new_l, pin, new_seq, flush_fn);

		if (old_l && old_l != new_l)
			spin_unlock(&old_l->lock);
		spin_unlock(&new_l->lock);

		if (!race) {
			/*
			 * If the journal is currently full,  we might want to call flush_fn
			 * immediately:
			 */
			if (new_seq == j->last_seq)
				journal_wake(j);
			if (reclaim)
				bch2_journal_maybe_update_last_seq(j);
			break;
		}
	}
}

/**
 * bch2_journal_pin_flush: ensure journal pin callback is no longer running
 * @j:		journal object
 * @pin:	pin to flush
 */
void bch2_journal_pin_flush(struct journal *j, struct journal_entry_pin *pin)
{
	BUG_ON(journal_pin_active(pin));

	wait_event(j->pin_flush_wait, j->flush_in_progress != pin);
}

/*
 * Journal reclaim: flush references to open journal entries to reclaim space in
 * the journal
 *
 * May be done by the journal code in the background as needed to free up space
 * for more journal entries, or as part of doing a clean shutdown, or to migrate
 * data off of a specific device:
 */

static struct journal_entry_pin *
journal_get_next_pin(struct journal *j,
		     u64 seq_to_flush,
		     unsigned allowed_below_seq,
		     unsigned allowed_above_seq,
		     struct journal_entry_pin **skipped,
		     unsigned nr_skipped,
		     u64 *seq, journal_pin_flush_fn *flush_fn)
{
	guard(percpu_read)(&j->pin_resize_lock);

	struct journal_entry_pin_list *pin_list;
	struct journal_entry_pin *ret = NULL;

	fifo_for_each_entry_ptr(pin_list, &j->pin, *seq) {
		guard(spinlock)(&pin_list->lock);

		/*
		 * Flushing journal pins (writing btree nodes) requires
		 * consuming journal space: don't get ahead of journal replay to
		 * avoid deadlocking
		 */
		if (pin_list->unreplayed)
			break;

		if (*seq > seq_to_flush && !allowed_above_seq)
			break;

		for (unsigned i = 0; i < JOURNAL_PIN_TYPE_NR; i++)
			if (((BIT(i) & allowed_below_seq) && *seq <= seq_to_flush) ||
			    (BIT(i) & allowed_above_seq)) {
				list_for_each_entry(ret, &pin_list->unflushed[i], list) {
					if (journal_pin_is_skipped(ret, skipped, nr_skipped))
						continue;
					BUG_ON(j->flush_in_progress);
					j->flush_in_progress = ret;
					j->flush_in_progress_dropped = false;
					*flush_fn = ret->flush;
					return ret;
				}
			}
	}

	return NULL;
}

/* returns true if we did work */
static size_t journal_flush_pins(struct journal *j,
				 u64 seq_to_flush,
				 unsigned allowed_below_seq,
				 unsigned allowed_above_seq,
				 unsigned min_any,
				 unsigned min_key_cache)
{
	struct bch_fs *c = container_of(j, struct bch_fs, journal);
	struct journal_entry_pin *pin;
	size_t nr_flushed = 0;
	/*
	 * journal_reclaim_would_deadlock means "this pin can't be flushed from
	 * reclaim right now without blocking behind another transaction". Keep a
	 * bounded per-pass skip set so one contended old pin doesn't hide other
	 * flushable pins; skipped pins stay unflushed and are retried next pass.
	 */
	struct journal_entry_pin *deadlock_skipped[JOURNAL_PIN_DEADLOCK_SKIP_MAX];
	unsigned nr_deadlock_skipped = 0;
	int err;

	lockdep_assert_held(&j->reclaim_lock);

	while (1) {
		unsigned allowed_above = allowed_above_seq;
		unsigned allowed_below = allowed_below_seq;

		if (min_any) {
			allowed_above |= ~0;
			allowed_below |= ~0;
		}

		if (min_key_cache) {
			allowed_above |= BIT(JOURNAL_PIN_TYPE_key_cache);
			allowed_below |= BIT(JOURNAL_PIN_TYPE_key_cache);
		}

		cond_resched();

		j->last_flushed = jiffies;

		/*
		 * Hold pin_resize_lock for the entire grab→flush→clear context.
		 * journal_get_next_pin returns a pin and stashes it in
		 * j->flush_in_progress; the post-flush re-lookup of pin_l by seq
		 * (and the spinlock + list_move that follows) must see the same
		 * pin.data/mask snapshot, so resize can't run mid-iteration.
		 */
		u64 seq;
		journal_pin_flush_fn flush_fn;
		pin = journal_get_next_pin(j, seq_to_flush, allowed_below, allowed_above,
					   deadlock_skipped, nr_deadlock_skipped,
					   &seq, &flush_fn);
		if (!pin)
			break;

		bool key_cache_pin = flush_fn == bch2_btree_key_cache_journal_flush;
		u64 start_time = local_clock();
		bool retry_deadlocked = false;
		err = flush_fn(j, pin, seq);

		scoped_guard(percpu_read, &j->pin_resize_lock) {
			struct journal_entry_pin_list *pin_l = &fifo_entry(&j->pin, seq);

			guard(spinlock)(&pin_l->lock);
			enum journal_pin_type type = journal_pin_type(pin, flush_fn);
			bool would_deadlock =
				bch2_err_matches(err, BCH_ERR_journal_reclaim_would_deadlock);

			enum bch_time_stats flush_time =
				type <= JOURNAL_PIN_TYPE_btree0
				? BCH_TIME_journal_pin_flush_btree
				: type == JOURNAL_PIN_TYPE_key_cache
				? BCH_TIME_journal_pin_flush_key_cache
				: BCH_TIME_journal_pin_flush_other;
			bch2_time_stats_update(&c->times[flush_time], start_time);

			/* Pin might have been dropped or rearmed: */
			if (likely(!err && !j->flush_in_progress_dropped))
				list_move(&pin->list, &pin_l->flushed);
			else if (would_deadlock && !j->flush_in_progress_dropped &&
				 nr_deadlock_skipped < ARRAY_SIZE(deadlock_skipped)) {
				deadlock_skipped[nr_deadlock_skipped++] = pin;
				retry_deadlocked = true;
			}
			j->flush_in_progress = NULL;
			j->flush_in_progress_dropped = false;
		}

		wake_up(&j->pin_flush_wait);

		if (err) {
			if (retry_deadlocked)
				continue;
			break;
		}

		if (min_key_cache && key_cache_pin)
			min_key_cache--;

		if (min_any)
			min_any--;

		nr_flushed++;
	}

	return nr_flushed;
}

static u64 journal_seq_to_flush(struct journal *j)
{
	struct bch_fs *c = container_of(j, struct bch_fs, journal);
	u64 seq_to_flush = 0;

	guard(spinlock)(&j->lock);
	guard(rcu)();

	for_each_rw_member_rcu(c, ca) {
		struct journal_device *ja = &ca->journal;
		unsigned nr_buckets, bucket_to_flush;

		if (!ja->nr)
			continue;

		/* Try to keep the journal at most half full: */
		nr_buckets = ja->nr / 2;

		bucket_to_flush = (ja->cur_idx + nr_buckets) % ja->nr;
		seq_to_flush = max(seq_to_flush,
				   ja->bucket_seq[bucket_to_flush]);
	}

	/* Also flush if the pin fifo is more than half full */
	return max_t(s64, seq_to_flush,
		     (s64) journal_cur_seq(j) -
		     (j->pin.size >> 1));
}

/**
 * __bch2_journal_reclaim - free up journal buckets
 * @j:		journal object
 * @direct:	direct or background reclaim?
 * @kicked:	requested to run since we last ran?
 *
 * Background journal reclaim writes out btree nodes. It should be run
 * early enough so that we never completely run out of journal buckets.
 *
 * High watermarks for triggering background reclaim:
 * - FIFO has fewer than 512 entries left
 * - fewer than 25% journal buckets free
 *
 * Background reclaim runs until low watermarks are reached:
 * - FIFO has more than 1024 entries left
 * - more than 50% journal buckets free
 *
 * As long as a reclaim can complete in the time it takes to fill up
 * 512 journal entries or 25% of all journal buckets, then
 * journal_next_bucket() should not stall.
 */
static int __bch2_journal_reclaim(struct journal *j, bool direct, bool kicked)
{
	struct bch_fs *c = container_of(j, struct bch_fs, journal);
	struct bch_fs_btree_cache *bc = &c->btree.cache;
	bool kthread = (current->flags & PF_KTHREAD) != 0;
	u64 seq_to_flush;
	size_t min_nr, min_key_cache, nr_flushed;
	int ret = 0;

	/*
	 * We can't invoke memory reclaim while holding the reclaim_lock -
	 * journal reclaim is required to make progress for memory reclaim
	 * (cleaning the caches), so we can't get stuck in memory reclaim while
	 * we're holding the reclaim lock:
	 */
	lockdep_assert_held(&j->reclaim_lock);
	guard(memalloc_flags)(PF_MEMALLOC_NOIO);

	do {
		if (kthread && kthread_should_stop())
			break;

		ret = bch2_journal_error(j);
		if (ret)
			break;

		closure_wait_event(&bc->nr_in_flight_wait,
				   atomic_long_read(&bc->nr_in_flight_inner) < BTREE_WRITE_IO_LIMIT(c));

		seq_to_flush = journal_seq_to_flush(j);
		min_nr = 0;

		/*
		 * If it's been longer than j->reclaim_delay_ms since we last flushed,
		 * make sure to flush at least one journal pin:
		 */
		if (time_after(jiffies, j->last_flushed +
			       msecs_to_jiffies(c->opts.journal_reclaim_delay)))
			min_nr = 1;

		if (journal_med_on_space(j))
			min_nr = 1;

		size_t btree_cache_live = btree_cache_list_nr(&bc->live[0]) +
					  btree_cache_list_nr(&bc->live[1]);
		size_t btree_cache_dirty = bc->live[0].nr_dirty + bc->live[1].nr_dirty;
		if (btree_cache_dirty * 2 > btree_cache_live)
			min_nr = 1;

		min_key_cache = min(bch2_nr_btree_keys_need_flush(c), (size_t) 128);

		event_inc_trace(c, journal_reclaim_start, buf, ({
			prt_printf(&buf, "direct %u kicked %u\n", direct, kicked);
			prt_printf(&buf, "btree cache %zu/%zu min %zu\n",
				   btree_cache_dirty, btree_cache_live, min_nr);
			prt_printf(&buf, "key cache %lu/%lu min %zu\n",
				   atomic_long_read(&c->btree.key_cache.nr_dirty),
				   atomic_long_read(&c->btree.key_cache.nr_keys),
				   min_key_cache);
		}));

		nr_flushed = journal_flush_pins(j, seq_to_flush,
						~0, 0,
						min_nr, min_key_cache);

		if (direct)
			j->nr_direct_reclaim += nr_flushed;
		else
			j->nr_background_reclaim += nr_flushed;

		event_inc_trace(c, journal_reclaim_finish, buf,
			prt_printf(&buf, "flushed %zu\n", nr_flushed));

		if (nr_flushed)
			wake_up(&j->reclaim_wait);
	} while ((min_nr || min_key_cache) && nr_flushed && !direct);

	return ret;
}

int bch2_journal_reclaim(struct journal *j)
{
	return __bch2_journal_reclaim(j, true, true);
}

static int bch2_journal_reclaim_thread(void *arg)
{
	struct journal *j = arg;
	struct bch_fs *c = container_of(j, struct bch_fs, journal);
	unsigned long delay, now;
	bool journal_empty;
	int ret = 0;

	set_freezable();

	kthread_wait_freezable(test_bit(BCH_FS_rw, &c->flags) ||
			       kthread_should_stop());

	j->last_flushed = jiffies;

	while (!ret && !kthread_should_stop()) {
		bool kicked = j->reclaim_kicked;

		j->reclaim_kicked = false;

		scoped_guard(mutex, &j->reclaim_lock)
			ret = __bch2_journal_reclaim(j, false, kicked);

		now = jiffies;
		delay = msecs_to_jiffies(c->opts.journal_reclaim_delay);
		j->next_reclaim = j->last_flushed + delay;

		if (!time_in_range(j->next_reclaim, now, now + delay))
			j->next_reclaim = now + delay;

		while (1) {
			set_current_state(TASK_INTERRUPTIBLE|TASK_FREEZABLE);
			if (kthread_should_stop())
				break;
			if (j->reclaim_kicked)
				break;

			scoped_guard(spinlock, &j->lock)
				journal_empty = fifo_empty(&j->pin);

			long timeout = j->next_reclaim - jiffies;

			if (journal_empty)
				schedule();
			else if (timeout > 0)
				schedule_timeout(timeout);
			else
				break;
		}
		__set_current_state(TASK_RUNNING);
	}

	return 0;
}

void bch2_journal_reclaim_stop(struct journal *j)
{
	struct task_struct *p = j->reclaim_thread;

	j->reclaim_thread = NULL;

	if (p) {
		kthread_stop(p);
		put_task_struct(p);
	}
}

int bch2_journal_reclaim_start(struct journal *j)
{
	struct bch_fs *c = container_of(j, struct bch_fs, journal);
	struct task_struct *p;
	int ret;

	if (j->reclaim_thread) {
		wake_up_process(j->reclaim_thread);
		return 0;
	}

	p = kthread_create(bch2_journal_reclaim_thread, j,
			   "bch-reclaim/%s", c->name);
	ret = PTR_ERR_OR_ZERO(p);
	bch_err_msg(c, ret, "creating journal reclaim thread");
	if (ret)
		return ret;

	get_task_struct(p);
	j->reclaim_thread = p;
	wake_up_process(p);
	return 0;
}

static bool journal_pins_still_flushing(struct journal *j, u64 seq_to_flush,
					unsigned types)
{
	guard(percpu_read)(&j->pin_resize_lock);
	u64 start = READ_ONCE(j->pin.front);
	u64 end = READ_ONCE(j->pin.back);

	if (seq_to_flush != U64_MAX)
		end = min(end, seq_to_flush + 1);

	for (u64 seq = start; seq < end; seq++) {
		struct journal_entry_pin_list *pin_list = &fifo_entry(&j->pin, seq);

		guard(spinlock)(&pin_list->lock);

		for (unsigned i = 0; i < JOURNAL_PIN_TYPE_NR; i++)
			if ((BIT(i) & types) &&
			    !list_empty(&pin_list->unflushed[i]))
				return true;

		struct journal_entry_pin *pin;
		list_for_each_entry(pin, &pin_list->flushed, list)
			if (BIT(journal_pin_type(pin, pin->flush)) & types)
				return true;

	}

	return false;
}

static bool journal_flush_pins_or_still_flushing(struct journal *j, u64 seq_to_flush,
						 unsigned types)
{
	return  journal_flush_pins(j, seq_to_flush, types, 0, 0, 0) ||
		journal_pins_still_flushing(j, seq_to_flush, types);
}

static int journal_flush_done(struct journal *j, u64 seq_to_flush,
			      bool *did_work)
{
	try(bch2_journal_error(j));

	guard(mutex)(&j->reclaim_lock);

	for (int type = JOURNAL_PIN_TYPE_NR - 1;
	     type >= 0;
	     --type)
		if (journal_flush_pins_or_still_flushing(j, seq_to_flush, BIT(type))) {
			*did_work = true;

			/*
			 * Question from Dan Carpenter, on the early return:
			 *
			 * If journal_flush_pins_or_still_flushing() returns
			 * true, then the flush hasn't complete and we must
			 * return 0; we want the outer closure_wait_event() in
			 * journal_flush_pins() to continue.
			 *
			 * The early return is there because we don't want to
			 * cycle the journal until we've finished
			 * flushing all outstanding journal pins - otherwise
			 * seq_to_flush can be U64_MAX, and we'll close a bunch
			 * of journal entries and write tiny ones completely
			 * unnecessarily.
			 *
			 * Having the early return be in the loop where we loop
			 * over types is important, because flushing one journal
			 * pin can cause new journal pins to be added (even of
			 * the same type, btree node writes may generate more
			 * btree node writes, when updating the parent pointer
			 * has a full node and has to trigger a split/compact).
			 *
			 * This is part of our shutdown sequence, where order of
			 * flushing is important in order to make sure that it
			 * terminates...
			 */
			return 0;
		}

	if (seq_to_flush >= journal_cur_seq(j))
		bch2_journal_cycle(j, JOURNAL_CYCLE_must_close);

	/*
	 * If journal replay hasn't completed, the unreplayed journal entries
	 * hold refs on their corresponding sequence numbers
	 */
	guard(spinlock)(&j->lock);
	return !test_bit(JOURNAL_replay_done, &j->flags) ||
		j->last_seq > seq_to_flush ||
		j->last_seq == j->pin.back;
}

bool bch2_journal_flush_pins(struct journal *j, u64 seq_to_flush)
{
	/* time_stats this */
	bool did_work = false;

	if (!test_bit(JOURNAL_running, &j->flags))
		return false;

	closure_wait_event(&j->reclaim_flush_wait,
		journal_flush_done(j, seq_to_flush, &did_work));

	return did_work;
}

int bch2_journal_flush_device_pins(struct journal *j, int dev_idx)
{
	struct bch_fs *c = container_of(j, struct bch_fs, journal);
	struct journal_entry_pin_list *p;
	u64 iter, seq = 0;

	scoped_guard(percpu_read, &j->pin_resize_lock)
	scoped_guard(spinlock, &j->lock)
		fifo_for_each_entry_ptr(p, &j->pin, iter)
			if (dev_idx >= 0
			    ? journal_pin_has_dev(p, dev_idx)
			    : p->devs.nr < c->opts.metadata_replicas)
				seq = iter;

	bch2_journal_flush_pins(j, seq);

	try(bch2_journal_error(j));

	/*
	 * flush_pins() only advances the in-memory last_seq past @seq. The
	 * device's journal replicas entries aren't dropped until last_seq_ondisk
	 * advances past them (in journal_write_done()), and that requires a
	 * journal write recording the new last_seq. Force one here so we don't
	 * depend on a caller flushing afterwards: otherwise dev_idx lingers in
	 * the on-disk journal replicas set, and once it's taken offline the next
	 * superblock write can't satisfy that entry and goes emergency read-only.
	 */
	try(bch2_journal_meta(j));

	return 0;
}

/*
 * Before a device leaves the journal write set (going RO/evacuating), push
 * journal reclaim until the *other* journal devices have free space to write
 * to. Otherwise, if @dev_idx held the journal's only free space, dropping it
 * from rw_devs[journal] strands the journal: reclaim can no longer advance
 * last_seq_ondisk (which needs a journal write, hence a writable device) to
 * free anyone else's buckets, and we deadlock in journal_full - with the task
 * taking the device offline wedged in journal_res_get while holding state_lock.
 *
 * Must run before @dev_idx is pulled from rw_devs[journal], so reclaim can
 * still use it to write the last_seq advance that frees the remaining devices.
 * Can't fail: the only way out of the loop besides success is the whole
 * filesystem going read-only, which stops reclaim.
 */
void bch2_journal_flush_dev_ro(struct journal *j, unsigned dev_idx)
{
	struct bch_fs *c = container_of(j, struct bch_fs, journal);
	unsigned want = READ_ONCE(c->opts.metadata_replicas);

	while (true) {
		unsigned nr_devs = 0, nr_with_space = 0;
		bool flush_needed;

		scoped_guard(spinlock, &j->lock) {
			scoped_guard(rcu)
				for_each_member_device_rcu(c, ca,
						&c->allocator.rw_devs[BCH_DATA_journal]) {
					if (ca->dev_idx == dev_idx || !ca->journal.nr)
						continue;
					nr_devs++;
					if (bch2_journal_dev_buckets_available(j, &ca->journal,
								journal_space_discarded))
						nr_with_space++;
				}

			/*
			 * Reclaim advances last_seq (in memory) as it flushes
			 * pins, but the buckets it frees can't be discarded until
			 * last_seq_ondisk catches up, which only happens when a
			 * journal write completes (journal_write_done()). Only
			 * force a write when there's such an advance to persist.
			 */
			flush_needed = j->last_seq_ondisk < j->last_seq;
		}

		/*
		 * Enough other devices have free journal space that the journal
		 * can keep writing once @dev_idx leaves the write set:
		 */
		if (nr_with_space >= min(want, nr_devs))
			break;

		int ret;
		if (flush_needed) {
			ret = bch2_journal_meta(j);
		} else {
			scoped_guard(mutex, &j->reclaim_lock)
				ret = bch2_journal_reclaim(j);
		}
		if (ret)	/* filesystem read-only */
			break;
	}
}

__cold bool bch2_journal_seq_pins_to_text(struct printbuf *out, struct journal *j, u64 *seq,
					  unsigned *nr)
{
	struct journal_entry_pin *pin;

	if (!test_bit(JOURNAL_running, &j->flags))
		return true;

	*seq = max(*seq, j->last_seq);

	if (*seq >= j->pin.back)
		return true;

	guard(percpu_read)(&j->pin_resize_lock);
	struct journal_entry_pin_list *pin_list = &fifo_entry(&j->pin, *seq);
	guard(spinlock)(&pin_list->lock);
	guard(printbuf_atomic)(out);

	prt_printf(out, "%llu: count %u", *seq, atomic_read(&pin_list->count));
	if (pin_list->unreplayed)
		prt_str(out, " unreplayed");
	prt_newline(out);
	guard(printbuf_indent)(out);

	union bch_replicas_padded devs_r;
	journal_pin_devs_to_replicas(&devs_r, pin_list);
	bch2_replicas_entry_to_text(out, &devs_r.e);
	prt_newline(out);

	prt_printf(out, "unflushed:\n");
	for (unsigned i = 0; i < ARRAY_SIZE(pin_list->unflushed); i++)
		list_for_each_entry(pin, &pin_list->unflushed[i], list) {
			prt_printf(out, "\t%px %ps\n", pin, pin->flush);
			if (nr)
				++*nr;
		}

	prt_printf(out, "flushed:\n");
	list_for_each_entry(pin, &pin_list->flushed, list) {
		prt_printf(out, "\t%px %ps\n", pin, pin->flush);
		if (nr)
			++*nr;
	}

	return false;
}

__cold void bch2_journal_pins_to_text(struct printbuf *out, struct journal *j,
				      unsigned limit)
{
	u64 seq = 0;
	unsigned nr = 0;

	/* limit the number of pins printed, not the number of seqs scanned */
	while (nr < limit && !bch2_journal_seq_pins_to_text(out, j, &seq, &nr))
		seq++;
}

static __cold void bch2_time_stats_summary_to_text(struct printbuf *out,
					    const char *name,
					    struct bch2_time_stats *stats)
{
	prt_printf(out, "%s:\t%llu\t", name, stats->duration_stats.n);
	bch2_pr_time_units(out, mean_and_variance_get_mean(stats->duration_stats));
	prt_tab(out);
	bch2_pr_time_units(out, stats->max_duration);
	prt_newline(out);
}

__cold void bch2_journal_reclaim_to_text(struct printbuf *out, struct journal *j)
{
	struct bch_fs *c = container_of(j, struct bch_fs, journal);
	unsigned long now = jiffies;

	if (!out->nr_tabstops)
		printbuf_tabstop_push(out, 36);

	prt_printf(out, "nr direct reclaim:\t%llu\n",		j->nr_direct_reclaim);
	prt_printf(out, "nr background reclaim:\t%llu\n",	j->nr_background_reclaim);
	prt_printf(out, "reclaim kicked:\t%u\n",		j->reclaim_kicked);
	prt_printf(out, "last flushed:\t%u ms ago\n",
		   jiffies_to_msecs(now - j->last_flushed));
	prt_printf(out, "next reclaim:\t%u ms\n",
		   time_after(j->next_reclaim, now)
		   ? jiffies_to_msecs(j->next_reclaim - now) : 0);

	struct journal_entry_pin *pin = READ_ONCE(j->flush_in_progress);
	if (pin)
		prt_printf(out, "flush in progress:\t%ps\n", pin->flush);

	printbuf_tabstops_reset(out);
	printbuf_tabstop_push(out, 24);
	printbuf_tabstop_push(out, 12);
	printbuf_tabstop_push(out, 12);

	prt_newline(out);
	prt_printf(out, "Pin flush time stats:\tcount\tavg\tmax\n");
	bch2_time_stats_summary_to_text(out, "  btree",     &c->times[BCH_TIME_journal_pin_flush_btree]);
	bch2_time_stats_summary_to_text(out, "  key_cache", &c->times[BCH_TIME_journal_pin_flush_key_cache]);
	bch2_time_stats_summary_to_text(out, "  other",     &c->times[BCH_TIME_journal_pin_flush_other]);

	prt_newline(out);
	prt_printf(out, "Blocked time stats:\tcount\tavg\tmax\n");
	bch2_time_stats_summary_to_text(out, "  low_on_space",		&c->times[BCH_TIME_blocked_journal_low_on_space]);
	bch2_time_stats_summary_to_text(out, "  low_on_pin",		&c->times[BCH_TIME_blocked_journal_low_on_pin]);
	bch2_time_stats_summary_to_text(out, "  max_in_flight",		&c->times[BCH_TIME_blocked_journal_max_in_flight]);
	bch2_time_stats_summary_to_text(out, "  max_open",		&c->times[BCH_TIME_blocked_journal_max_open]);
	bch2_time_stats_summary_to_text(out, "  write_buffer_full",	&c->times[BCH_TIME_blocked_write_buffer_full]);

	prt_newline(out);
	prt_printf(out, "Oldest journal pins:\n");
	bch2_journal_pins_to_text(out, j, 8);

	struct task_struct *t = READ_ONCE(j->reclaim_thread);
	if (t)
		get_task_struct(t);

	prt_newline(out);

	if (t) {
		prt_printf(out, "Reclaim thread:\n");
		scoped_guard(printbuf_indent, out)
			bch2_prt_task_backtrace(out, t, 0, GFP_KERNEL);
		put_task_struct(t);
	} else {
		prt_printf(out, "Reclaim thread not running\n");
	}
}
