/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_UTIL_SIX_TYPES_H
#define _BCACHEFS_UTIL_SIX_TYPES_H

#include "enum_kind.h"

enum __enum_closed six_lock_type {
	SIX_LOCK_read,
	SIX_LOCK_intent,
	SIX_LOCK_write,
};

struct six_lock_waiter {
	u64			trans_start_time;
	struct task_struct	*task;
	enum six_lock_type	lock_want;
	bool			lock_acquired;
	/* Index in wait_fifo->data[], set on insert, used for O(1) self-remove. */
	u16			slot_idx;
};

struct six_lock_wait_slot {
	struct six_lock_waiter	*w;
	u64			start_time;
};

/*
 * RCU-swappable wait list. Not a FIFO — entries sit at fixed indices from
 * insertion to removal, so RCU readers (the cycle detector) never observe an
 * entry moving. Removal writes NULL atomically; insertion finds a free slot
 * (starting from @next_free_hint) and fills it. @nr is the high-water-mark
 * index in use and shrinks when trailing slots go tombstone.
 *
 * Grown by allocating a new struct, copying fields + entries, then
 * rcu_assign_pointer'ing the lock's wait_fifo to the new object. Old
 * heap allocations are kvfree_rcu'd so future lockless readers can
 * outrun the free.
 */
struct six_lock_wait_fifo {
	u16			size;
	u16			nr;
	u16			next_free_hint;
	struct six_lock_wait_slot data[];
};

#define SIX_LOCK_INLINE_WAITERS	8

struct six_lock {
	atomic_t		state;
	u32			seq;
	unsigned __percpu	*readers;
	unsigned		intent_lock_recurse;
	unsigned		write_lock_recurse;
	struct task_struct	*owner;
#ifdef CONFIG_BCACHEFS_DEBUG
	bch_stacktrace		owner_stack;
#endif
	raw_spinlock_t		wait_lock;

	struct six_lock_wait_fifo __rcu *wait_fifo;

	struct six_lock_wait_fifo	inline_fifo;
	struct six_lock_wait_slot	inline_fifo_data[SIX_LOCK_INLINE_WAITERS];
#ifdef CONFIG_DEBUG_LOCK_ALLOC
	struct lockdep_map	dep_map;
#endif
};

typedef int (*six_lock_should_sleep_fn)(struct six_lock *lock, struct six_lock_waiter *);

enum __enum_flags six_lock_init_flags {
	SIX_LOCK_INIT_PCPU	= 1U << 0,
};

struct six_lock_count {
	unsigned n[3];
};

#endif /* _BCACHEFS_UTIL_SIX_TYPES_H */
