/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_JOURNAL_RECLAIM_TYPES_H
#define _BCACHEFS_JOURNAL_RECLAIM_TYPES_H

typedef struct {
	unsigned			nr_refs;
	union bch_replicas_padded	replicas;
} replicas_entry_refs;

DEFINE_DARRAY_PREALLOCATED(replicas_entry_refs, 16);

#endif /* _BCACHEFS_JOURNAL_RECLAIM_TYPES_H */
