/*
 * kobject.h - generic kernel object infrastructure.
 *
 * Copyright (c) 2002-2003 Patrick Mochel
 * Copyright (c) 2002-2003 Open Source Development Labs
 * Copyright (c) 2006-2008 Greg Kroah-Hartman <greg@kroah.com>
 * Copyright (c) 2006-2008 Novell Inc.
 *
 * This file is released under the GPLv2.
 *
 * Please read Documentation/kobject.txt before using the kobject
 * interface, ESPECIALLY the parts about reference counts and object
 * destructors.
 */

#ifndef _KOBJECT_H_
#define _KOBJECT_H_

#include <linux/atomic.h>
#include <linux/bug.h>
#include <linux/compiler.h>
#include <linux/kernel.h>
#include <linux/slab.h>
#include <linux/sysfs.h>
#include <linux/types.h>
#include <linux/workqueue.h>

#include "util/darray.h"

struct kobj_type {
	void (*release)(struct kobject *kobj);
	const struct sysfs_ops *sysfs_ops;
	const struct attribute_group **default_groups;
	const struct kobj_ns_type_operations *(*child_ns_type)(struct kobject *kobj);
	const void *(*namespace)(struct kobject *kobj);
};

struct kobj_uevent_env {
};

struct kobj_attribute {
	struct attribute attr;
	ssize_t (*show)(struct kobject *kobj, struct kobj_attribute *attr,
			char *buf);
	ssize_t (*store)(struct kobject *kobj, struct kobj_attribute *attr,
			 const char *buf, size_t count);
};

struct kobject {
	const char		*name;
	struct kobject		*parent;
	const struct kobj_type	*ktype;
	atomic_t		ref;
	bool			state_initialized:1;
	bool			state_in_sysfs:1;
	bool			state_add_uevent_sent:1;
	bool			state_remove_uevent_sent:1;
	bool			uevent_suppress:1;

	DARRAY(struct kobject *) subdirs;
	DARRAY(const struct attribute *) files;
	DARRAY(const struct bin_attribute *) bin_files;
};

enum kobject_action {
	KOBJ_ADD,
	KOBJ_REMOVE,
	KOBJ_CHANGE,
	KOBJ_MOVE,
	KOBJ_ONLINE,
	KOBJ_OFFLINE,
	KOBJ_BIND,
	KOBJ_UNBIND,
};

void kobject_init(struct kobject *, const struct kobj_type *);
struct kobject *kobject_get(struct kobject *);

int kobject_add(struct kobject *, struct kobject *, const char *, ...);
void kobject_del(struct kobject *);

void kobject_put(struct kobject *);
struct kobject *kobject_get(struct kobject *);

static inline void kobject_uevent_env(struct kobject *kobj, int flags, char **envp) {}

int sysfs_read_or_html_dirlist(const char *, struct printbuf *);
int sysfs_write(const char *, const char *, size_t);

#define fs_kobj	NULL

#endif /* _KOBJECT_H_ */
