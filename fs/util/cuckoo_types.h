/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_UTIL_CUCKOO_TYPES_H
#define _BCACHEFS_UTIL_CUCKOO_TYPES_H

#define CUCKOO_NR_HASH	3

struct cuckoo_u64 {
	u64		seeds[CUCKOO_NR_HASH];
	unsigned	bits;
	size_t		nr;
	u64		stash;
	u64		*d;
};

#endif /* _BCACHEFS_UTIL_CUCKOO_TYPES_H */
