/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BCACHEFS_TYPES_H
#define _BCACHEFS_BCACHEFS_TYPES_H

#include "enum_kind.h"

struct bch_log_msg {
	struct bch_fs	*c;
	u8		loglevel;
	struct printbuf	m;
};

enum __enum_closed kern_loglevels {
	LOGLEVEL_emerg		= 0,
	LOGLEVEL_alert		= 1,
	LOGLEVEL_crit		= 2,
	LOGLEVEL_err		= 3,
	LOGLEVEL_warning	= 4,
	LOGLEVEL_notice		= 5,
	LOGLEVEL_info		= 6,
	LOGLEVEL_debug		= 7,
};

#endif /* _BCACHEFS_BCACHEFS_TYPES_H */
