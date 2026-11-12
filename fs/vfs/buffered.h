/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_FS_IO_BUFFERED_H
#define _BCACHEFS_FS_IO_BUFFERED_H

#ifndef NO_BCACHEFS_FS

#include "data/write_gen.h"
#include "vfs/buffered_gen.h"

int bch2_read_folio(struct file *, struct folio *);

#endif

#endif /* _BCACHEFS_FS_IO_BUFFERED_H */
