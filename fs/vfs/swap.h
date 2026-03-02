/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_VFS_SWAP_H
#define _BCACHEFS_VFS_SWAP_H

#ifndef NO_BCACHEFS_FS

struct swap_info_struct;

int bch2_swap_activate(struct swap_info_struct *, struct file *, sector_t *);
void bch2_swap_deactivate(struct file *);
int bch2_swap_rw(struct kiocb *, struct iov_iter *);

#else

#endif /* NO_BCACHEFS_FS */
#endif /* _BCACHEFS_VFS_SWAP_H */
