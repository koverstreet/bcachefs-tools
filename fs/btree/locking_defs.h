/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BTREE_LOCKING_DEFS_H
#define _BCACHEFS_BTREE_LOCKING_DEFS_H

#include "enum_kind.h"

/* path lock state */

/* matches six lock types */
enum __enum_closed btree_node_locked_type {
	BTREE_NODE_UNLOCKED		= -1,
	BTREE_NODE_READ_LOCKED		= SIX_LOCK_read,
	BTREE_NODE_INTENT_LOCKED	= SIX_LOCK_intent,
	BTREE_NODE_WRITE_LOCKED		= SIX_LOCK_write,
};

#endif /* _BCACHEFS_BTREE_LOCKING_DEFS_H */
