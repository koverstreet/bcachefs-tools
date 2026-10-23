// SPDX-License-Identifier: GPL-2.0
#ifndef NO_BCACHEFS_CHARDEV

#include "bcachefs.h"

#include "init/chardev.h"
#include "init/chardev_gen.h"

#include <linux/cdev.h>
#include <linux/device.h>
#include <linux/fs.h>
#include <linux/major.h>

/*
 * bch_chardev_lock covers the minor -> filesystem lookup and the filesystem's
 * ro_ref tryget together: bch2_fs_stop() removes the minor under it before
 * waiting for ro_ref to drain, so an ioctl either holds a ref on a live
 * filesystem or doesn't find it - never one that's being torn down.
 */
static DEFINE_MUTEX(bch_chardev_lock);
static DEFINE_IDR(bch_chardev_minor);

static long bch2_chardev_ioctl(struct file *filp, unsigned cmd, unsigned long v)
{
	unsigned minor = iminor(file_inode(filp));
	void __user *arg = (void __user *) v;
	struct bch_fs *c = NULL;

	if (minor < U8_MAX) {
		scoped_guard(mutex, &bch_chardev_lock) {
			c = idr_find(&bch_chardev_minor, minor);
			if (c && !bch2_ro_ref_tryget(c))
				return bch2_err_class(bch_err_throw(c, ioctl_fs_stopping));
		}
	}

	if (!c)
		return bch2_global_ioctl(cmd, arg);

	long ret = bch2_fs_ioctl(c, cmd, arg);
	bch2_ro_ref_put(c);
	return ret;
}

static const struct file_operations bch_chardev_fops = {
	.owner		= THIS_MODULE,
	.unlocked_ioctl = bch2_chardev_ioctl,
	.open		= nonseekable_open,
};

static int bch_chardev_major;
static const struct class bch_chardev_class = {
	.name = "bcachefs",
};
static struct device *bch_chardev;

void bch2_fs_chardev_exit(struct bch_fs *c)
{
	if (!IS_ERR_OR_NULL(c->chardev))
		device_unregister(c->chardev);
	if (c->minor >= 0) {
		scoped_guard(mutex, &bch_chardev_lock)
			idr_remove(&bch_chardev_minor, c->minor);
	}
}

int bch2_fs_chardev_init(struct bch_fs *c)
{
	scoped_guard(mutex, &bch_chardev_lock)
		c->minor = idr_alloc(&bch_chardev_minor, c, 0, 0, GFP_KERNEL);
	if (c->minor < 0)
		return bch_err_throw(c, chardev_init_error);

	c->chardev = device_create(&bch_chardev_class, NULL,
				   MKDEV(bch_chardev_major, c->minor), c,
				   "bcachefs%u-ctl", c->minor);
	if (IS_ERR(c->chardev))
		return bch_err_throw(c, chardev_init_error);

	return 0;
}

void bch2_chardev_exit(void)
{
	device_destroy(&bch_chardev_class, MKDEV(bch_chardev_major, U8_MAX));
	class_unregister(&bch_chardev_class);
	if (bch_chardev_major > 0)
		unregister_chrdev(bch_chardev_major, "bcachefs");
}

int __init bch2_chardev_init(void)
{
	int ret;

	bch_chardev_major = register_chrdev(0, "bcachefs-ctl", &bch_chardev_fops);
	if (bch_chardev_major < 0)
		return bch_chardev_major;

	ret = class_register(&bch_chardev_class);
	if (ret)
		goto major_out;

	bch_chardev = device_create(&bch_chardev_class, NULL,
				    MKDEV(bch_chardev_major, U8_MAX),
				    NULL, "bcachefs-ctl");
	if (IS_ERR(bch_chardev)) {
		ret = PTR_ERR(bch_chardev);
		goto class_out;
	}

	return 0;

class_out:
	class_unregister(&bch_chardev_class);
major_out:
	unregister_chrdev(bch_chardev_major, "bcachefs-ctl");
	return ret;
}

#endif /* NO_BCACHEFS_CHARDEV */
