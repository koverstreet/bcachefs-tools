/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_UTIL_SEQMUTEX_TYPES_H
#define _BCACHEFS_UTIL_SEQMUTEX_TYPES_H

struct seqmutex {
	struct mutex	lock;
	u32		seq;
};

#endif /* _BCACHEFS_UTIL_SEQMUTEX_TYPES_H */
