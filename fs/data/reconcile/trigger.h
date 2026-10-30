/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_RECONCILE_TRIGGER_H
#define _BCACHEFS_RECONCILE_TRIGGER_H

#include "data/extents.h"
#include "data/reconcile/needs_reconcile_gen.h"
#include "data/reconcile/trigger_gen.h"
#include "fs/inode_opts_gen.h"

static inline enum reconcile_work_id rb_work_id_phys(enum reconcile_work_id w)
{
	return w == RECONCILE_WORK_pending ? RECONCILE_WORK_none : w;
}

#endif /* _BCACHEFS_RECONCILE_TRIGGER_H */
