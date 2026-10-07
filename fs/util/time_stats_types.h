/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_UTIL_TIME_STATS_TYPES_H
#define _BCACHEFS_UTIL_TIME_STATS_TYPES_H

struct time_unit {
	const char	*name;
	u64		nsecs;
};

/*
 * quantiles - do not use:
 *
 * Only enabled if bch2_time_stats->quantiles_enabled has been manually set - don't
 * use in new code.
 */

#define NR_QUANTILES	15

struct quantiles {
	struct quantile_entry {
		u64	m;
		u64	step;
	}		entries[NR_QUANTILES];
};

struct time_stat_buffer {
	/*
	 * Protects nr/entries on the owning cpu: irq/preempt disable on
	 * !PREEMPT_RT, a real per-cpu lock on RT - which is what makes
	 * taking stats->lock (sleeping on RT) from the buffer-full flush
	 * legal there. Cross-cpu readers (to_seq_buf, reset) don't take
	 * it; they race the owner by design, bounded by nr.
	 */
	local_lock_t	lock;
	unsigned	nr;
	struct time_stat_buffer_entry {
		u64	start;
		u64	end;
	}		entries[31];
};

struct bch2_time_stats {
	spinlock_t	lock;
	bool		have_quantiles;
	struct time_stat_buffer __percpu *buffer;
	/* all fields are in nanoseconds */
	u64             min_duration;
	u64		max_duration;
	u64		total_duration;
	u64             max_freq;
	u64             min_freq;
	u64		last_event;
	u64		last_event_start;
	u64		start_time;

	struct mean_and_variance	  duration_stats;
	struct mean_and_variance	  freq_stats;

/* default weight for streaming median+MAD: half-life ≈ 2^N samples */
#define TIME_STATS_MV_WEIGHT	8

	struct mean_and_variance	  duration_stats_weighted;
	struct mean_and_variance	  freq_stats_weighted;
};

struct bch2_time_stats_quantiles {
	struct bch2_time_stats	stats;
	struct quantiles	quantiles;
};

struct seq_buf;

#endif /* _BCACHEFS_UTIL_TIME_STATS_TYPES_H */
