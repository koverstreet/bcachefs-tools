/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DEBUG_H
#define _BCACHEFS_DEBUG_H

#include "bcachefs.h"

/* With CONFIG_DEBUG_FS, the debugfs entry points are debug.rs's, in here: */
#include "debug/debug_gen.h"

#ifndef CONFIG_DEBUG_FS
static inline void bch2_fs_debug_exit(struct bch_fs *c) {}
static inline void bch2_fs_debug_init(struct bch_fs *c) {}
static inline void bch2_debug_exit(void) {}
static inline int bch2_debug_init(void) { return 0; }
#endif

#endif /* _BCACHEFS_DEBUG_H */
