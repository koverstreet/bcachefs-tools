/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_FS_IO_BUFFERED_H
#define _BCACHEFS_FS_IO_BUFFERED_H

#ifndef NO_BCACHEFS_FS

#include "data/write_gen.h"
#include "vfs/buffered_gen.h"

int bch2_read_single_folio(struct folio *, struct address_space *, bool);
int bch2_read_folio(struct file *, struct folio *);

void bch2_readahead(struct readahead_control *);
#endif

#endif /* _BCACHEFS_FS_IO_BUFFERED_H */
