/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_ALLOC_DISK_GROUPS_DEFS_H
#define _BCACHEFS_ALLOC_DISK_GROUPS_DEFS_H

#include "enum_kind.h"

struct target {
	enum __enum_closed {
		TARGET_NULL,
		TARGET_DEV,
		TARGET_GROUP,
	}			type;
	union {
		unsigned	dev;
		unsigned	group;
	};
};

#endif /* _BCACHEFS_ALLOC_DISK_GROUPS_DEFS_H */
