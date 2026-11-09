/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_FS_IO_BUFFERED_H
#define _BCACHEFS_FS_IO_BUFFERED_H

#ifndef NO_BCACHEFS_FS

#include <linux/version.h>

#include "data/write_gen.h"
#include "vfs/buffered_gen.h"
#include "vfs/buffered_write_gen.h"
#include "vfs/writeback_gen.h"

int bch2_read_single_folio(struct folio *, struct address_space *, bool);
int bch2_read_folio(struct file *, struct folio *);
extern const struct address_space_operations bch_address_space_operations;

void bch2_readahead(struct readahead_control *);

#if LINUX_VERSION_CODE >= KERNEL_VERSION(6,17,0)
int bch2_write_begin(const struct kiocb *, struct address_space *, loff_t pos,
		     unsigned len, struct folio **, void **);
int bch2_write_end(const struct kiocb *, struct address_space *, loff_t,
		   unsigned len, unsigned copied, struct folio *, void *);
#else
int bch2_write_begin(struct file *, struct address_space *, loff_t pos,
		     unsigned len, struct folio **, void **);
int bch2_write_end(struct file *, struct address_space *, loff_t,
		   unsigned len, unsigned copied, struct folio *, void *);
#endif
#endif

#endif /* _BCACHEFS_FS_IO_BUFFERED_H */
