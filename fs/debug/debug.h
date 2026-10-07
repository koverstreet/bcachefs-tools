/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DEBUG_H
#define _BCACHEFS_DEBUG_H

#include "bcachefs.h"

#include "debug/debug_types.h"

void bch2_btree_node_ondisk_to_text(struct printbuf *, struct bch_fs *,
				    const struct btree *);

#ifdef CONFIG_DEBUG_FS

ssize_t bch2_debugfs_flush_buf(struct dump_iter *);
int bch2_dump_release(struct inode *, struct file *);

void bch2_fs_debug_exit(struct bch_fs *);
void bch2_fs_debug_init(struct bch_fs *);
void bch2_debug_exit(void);
int bch2_debug_init(void);
#else
static inline void bch2_fs_debug_exit(struct bch_fs *c) {}
static inline void bch2_fs_debug_init(struct bch_fs *c) {}
static inline void bch2_debug_exit(void) {}
static inline int bch2_debug_init(void) { return 0; }
#endif


#endif /* _BCACHEFS_DEBUG_H */
