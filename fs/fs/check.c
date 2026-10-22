// SPDX-License-Identifier: GPL-2.0
#include "bcachefs.h"

#include "fs/check.h"

/* translate to return code of fsck commad - man(8) fsck */
int bch2_fs_fsck_errcode(struct bch_fs *c, struct printbuf *msg)
{
	int ret = 0;

	if (test_bit(BCH_FS_errors_fixed, &c->flags)) {
		prt_printf(msg, "%s: errors fixed\n", c->name);
		ret |= 1;
	}
	if (test_bit(BCH_FS_error, &c->flags)) {
		prt_printf(msg, "%s: still has errors\n", c->name);
		ret |= 4;
	}
	if (test_bit(BCH_FS_emergency_ro, &c->flags)) {
		prt_printf(msg, "%s: fatal error (went emergency read-only)\n", c->name);
		ret |= 8;
	}

	return ret;
}
