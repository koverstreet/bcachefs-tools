#ifndef __LINUX_DCACHE_H
#define __LINUX_DCACHE_H

#include <stdbool.h>

struct super_block;
struct inode;

struct dentry {
	struct super_block	*d_sb;
	struct inode		*d_inode;
	bool			is_debugfs:1;
	const char		*name;
};

static inline void shrink_dcache_sb(struct super_block *sb) {}

#define QSTR_INIT(n,l) { { { .len = l } }, .name = n }
#define QSTR(n) (struct qstr)QSTR_INIT(n, strlen(n))

#endif	/* __LINUX_DCACHE_H */
