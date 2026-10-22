/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_FSCK_H
#define _BCACHEFS_FSCK_H

#include "str_hash.h"
#include "fs/check_gen.h"

/* The passes, each from its own file: */
#include "fs/check_inodes_gen.h"
#include "fs/check_extents_gen.h"
#include "fs/check_dirents_gen.h"
#include "fs/check_xattrs_gen.h"
#include "fs/check_root_gen.h"
#include "fs/check_dir_structure_gen.h"
#include "fs/check_unreachable_inodes_gen.h"
#include "fs/check_nlinks_gen.h"
#include "fs/fix_reflink_p_gen.h"

#endif /* _BCACHEFS_FSCK_H */
