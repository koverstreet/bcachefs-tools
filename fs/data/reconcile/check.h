/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_RECONCILE_CHECK_H
#define _BCACHEFS_RECONCILE_CHECK_H

void bch2_reconcile_rotational_changed(struct bch_fs *, struct bch_dev *);
int bch2_check_reconcile_work(struct bch_fs *);

#endif /* _BCACHEFS_RECONCILE_CHECK_H */
