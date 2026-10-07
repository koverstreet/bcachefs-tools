/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_FS_IO_DIRECT_H
#define _BCACHEFS_FS_IO_DIRECT_H

#ifndef NO_BCACHEFS_FS
#include "data/read.h"
#include "vfs/io.h"
#include "vfs/direct_gen.h"

int bch2_direct_IO_read(struct kiocb *, struct iov_iter *, enum bch_read_flags,
			struct bch_read_err_report *);
ssize_t bch2_direct_write(struct kiocb *, struct iov_iter *);
ssize_t bch2_read_iter(struct kiocb *, struct iov_iter *);
#endif

#endif /* _BCACHEFS_FS_IO_DIRECT_H */
