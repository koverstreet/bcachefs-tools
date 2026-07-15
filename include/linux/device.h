#ifndef _DEVICE_H_
#define _DEVICE_H_

#include <linux/slab.h>
#include <linux/types.h>

struct module;

struct class {
};

static inline void class_destroy(struct class *class)
{
	kfree(class);
}

static inline struct class * __must_check class_create(struct module *owner,
						       const char *name)
{
	return kzalloc(sizeof(struct class), GFP_KERNEL);
}

struct device {
};

static inline void device_unregister(struct device *dev)
{
	kfree(dev);
}

static inline void device_destroy(struct class *cls, dev_t devt) {}

static inline struct device *device_create(struct class *cls, struct device *parent,
			     dev_t devt, void *drvdata,
			     const char *fmt, ...)
{
	return kzalloc(sizeof(struct device), GFP_KERNEL);
}

#endif /* _DEVICE_H_ */
