/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_UTIL_FAST_LIST_TYPES_H
#define _BCACHEFS_UTIL_FAST_LIST_TYPES_H

struct fast_list_pcpu;

struct fast_list {
	GENRADIX(void *)	items;
	struct ida		slots_allocated;
	struct fast_list_pcpu __percpu
				*buffer;
};

#endif /* _BCACHEFS_UTIL_FAST_LIST_TYPES_H */
