/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_SB_MEMBERS_DEFS_H
#define _BCACHEFS_SB_MEMBERS_DEFS_H

struct sb_write;

struct bch_dev_identity {
	char name[sizeof(((struct bch_member *) NULL)->device_name) + 1];
	char model[sizeof(((struct bch_member *) NULL)->device_model) + 1];
	char serial[sizeof(((struct bch_member *) NULL)->device_serial) + 1];
	bool rotational;
};

#endif /* _BCACHEFS_SB_MEMBERS_DEFS_H */
