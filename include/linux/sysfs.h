#ifndef _SYSFS_H_
#define _SYSFS_H_

#include <linux/compiler.h>

struct file;
struct kobject;

struct attribute {
	const char		*name;
	umode_t			mode;
};

struct attribute_group {
	struct attribute	**attrs;
};

struct bin_attribute {
	struct attribute	attr;
	size_t			size;
	void			*private;
	ssize_t (*read)(struct file *, struct kobject *, const struct bin_attribute *,
			char *, loff_t, size_t);
	ssize_t (*write)(struct file *, struct kobject *, const struct bin_attribute *,
			 char *, loff_t, size_t);
};

struct sysfs_ops {
	ssize_t	(*show)(struct kobject *, struct attribute *, char *);
	ssize_t	(*store)(struct kobject *, struct attribute *, const char *, size_t);
};

int sysfs_create_file(struct kobject *, const struct attribute *);
int sysfs_create_bin_file(struct kobject *, const struct bin_attribute *);
void sysfs_remove_bin_file(struct kobject *, const struct bin_attribute *);

static inline int sysfs_create_link(struct kobject *kobj,
				    struct kobject *target, const char *name)
{
	return 0;
}

static inline void sysfs_remove_link(struct kobject *kobj, const char *name)
{
}

#endif /* _SYSFS_H_ */
