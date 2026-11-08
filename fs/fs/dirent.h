/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DIRENT_H
#define _BCACHEFS_DIRENT_H

#include "btree/iter.h"
#include "btree/update.h"
#include "data/checksum.h"
#include "fs/inode.h"
#include "init/error.h"
#include "data_structures/darray.h"
#include "snapshots/types_gen.h"
#include "snapshots/subvolume_gen.h"
#include "util/siphash_gen.h"
#include "fs/str_hash_gen.h"

#include "fs/dirent_gen.h"

static inline unsigned vfs_d_type(unsigned type)
{
	return type == DT_SUBVOL ? DT_DIR : type;
}

void bch2_dirent_init(void);
void bch2_filldir64_specialization_to_text(struct printbuf *);

#endif /* _BCACHEFS_DIRENT_H */
