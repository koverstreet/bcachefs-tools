/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_JOURNAL_READ_TYPES_H
#define _BCACHEFS_JOURNAL_READ_TYPES_H

typedef struct u64_range {
	u64	start;
	u64	end;
} u64_range;

DEFINE_DARRAY(u64_range);

#endif /* _BCACHEFS_JOURNAL_READ_TYPES_H */
