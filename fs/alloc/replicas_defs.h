/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_ALLOC_REPLICAS_DEFS_H
#define _BCACHEFS_ALLOC_REPLICAS_DEFS_H

#include "enum_kind.h"

enum __enum_closed bch_write_check {
	/* Starting: is there anywhere to write each data type at all? */
	BCH_WRITE_CHECK_start,
	/*
	 * A device leaving the rw set: also refuse, unless the matching force
	 * flag is set, if that takes a data type below its configured replicas
	 */
	BCH_WRITE_CHECK_dev_leaving_rw,
};

#endif /* _BCACHEFS_ALLOC_REPLICAS_DEFS_H */
