/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_UTIL_TWO_STATE_SHARED_LOCK_TYPES_H
#define _BCACHEFS_UTIL_TWO_STATE_SHARED_LOCK_TYPES_H

/*
 * Two-state lock - can be taken for add or block - both states are shared,
 * like read side of rwsem, but conflict with other state:
 */
typedef struct {
	atomic_long_t		v;
	wait_queue_head_t	wait;
} two_state_lock_t;

#endif /* _BCACHEFS_UTIL_TWO_STATE_SHARED_LOCK_TYPES_H */
