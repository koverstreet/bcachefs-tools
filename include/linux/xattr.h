/*
  File: linux/xattr.h

  Extended attributes handling.

  Copyright (C) 2001 by Andreas Gruenbacher <a.gruenbacher@computer.org>
  Copyright (c) 2001-2002 Silicon Graphics, Inc.  All Rights Reserved.
  Copyright (c) 2004 Red Hat, Inc., James Morris <jmorris@redhat.com>
*/
#ifndef _LINUX_XATTR_H
#define _LINUX_XATTR_H


#include <linux/slab.h>
#include <linux/types.h>
#include <linux/spinlock.h>
#include <uapi/linux/xattr.h>

#ifndef XATTR_CREATE
#define XATTR_CREATE	0x1
#endif

#ifndef XATTR_REPLACE
#define XATTR_REPLACE	0x2
#endif

struct inode;
struct dentry;
struct user_namespace;

/*
 * struct xattr_handler: When @name is set, match attributes with exactly that
 * name.  When @prefix is set instead, match attributes with that prefix and
 * with a non-empty suffix.
 */
struct xattr_handler {
	const char *name;
	const char *prefix;
	int flags;      /* fs private flags */
	bool (*list)(struct dentry *dentry);
	int (*get)(const struct xattr_handler *, struct dentry *dentry,
		   struct inode *inode, const char *name, void *buffer,
		   size_t size);
	int (*set)(const struct xattr_handler *,
		   struct user_namespace *mnt_userns, struct dentry *dentry,
		   struct inode *inode, const char *name, const void *buffer,
		   size_t size, int flags);
};

const char *xattr_full_name(const struct xattr_handler *, const char *);

struct xattr {
	const char *name;
	void *value;
	size_t value_len;
};

ssize_t xattr_getsecurity(struct inode *, const char *, void *, size_t);
ssize_t vfs_getxattr(struct dentry *, const char *, void *, size_t);
ssize_t vfs_listxattr(struct dentry *d, char *list, size_t size);
int __vfs_setxattr_noperm(struct dentry *, const char *, const void *, size_t, int);
int vfs_setxattr(struct dentry *, const char *, const void *, size_t, int);
int vfs_removexattr(struct dentry *, const char *);

ssize_t generic_getxattr(struct dentry *dentry, struct inode *inode, const char *name, void *buffer, size_t size);
ssize_t generic_listxattr(struct dentry *dentry, char *buffer, size_t buffer_size);
int generic_setxattr(struct dentry *dentry, struct inode *inode,
		     const char *name, const void *value, size_t size, int flags);
int generic_removexattr(struct dentry *dentry, const char *name);
ssize_t vfs_getxattr_alloc(struct dentry *dentry, const char *name,
			   char **xattr_value, size_t size, gfp_t flags);

static inline const char *xattr_prefix(const struct xattr_handler *handler)
{
	return handler->prefix ?: handler->name;
}

#endif	/* _LINUX_XATTR_H */
