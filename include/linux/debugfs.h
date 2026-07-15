/*
 *  debugfs.h - a tiny little debug file system
 *
 *  Copyright (C) 2004 Greg Kroah-Hartman <greg@kroah.com>
 *  Copyright (C) 2004 IBM Inc.
 *
 *	This program is free software; you can redistribute it and/or
 *	modify it under the terms of the GNU General Public License version
 *	2 as published by the Free Software Foundation.
 *
 *  debugfs is for people to use instead of /proc or /sys.
 *  See Documentation/DocBook/filesystems for more details.
 */

#ifndef _DEBUGFS_H_
#define _DEBUGFS_H_

#include <linux/fs.h>
#include <linux/seq_file.h>
#include <linux/types.h>
#include <linux/compiler.h>

struct file_operations;

struct dentry *debugfs_create_file(const char *, umode_t,
				   struct dentry *, void *,
				   const struct file_operations *);

struct dentry *debugfs_create_dir(const char *, struct dentry *);

void debugfs_remove(struct dentry *);
void debugfs_remove_recursive(struct dentry *);

#endif
