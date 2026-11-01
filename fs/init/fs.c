// SPDX-License-Identifier: GPL-2.0
/*
 * bcachefs setup/teardown code, and some metadata io - read a superblock and
 * figure out what to do with it.
 *
 * Copyright 2010, 2011 Kent Overstreet <kent.overstreet@gmail.com>
 * Copyright 2012 Google, Inc.
 */

#include "bcachefs.h"

#include "alloc/background.h"
#include "alloc/backpointers.h"
#include "alloc/discard.h"
#include "alloc/disk_groups.h"
#include "alloc/foreground.h"
#include "alloc/replicas.h"

#include "btree/check.h"
#include "btree/journal_overlay.h"
#include "btree/init.h"
#include "btree/interior.h"
#include "btree/key_cache.h"
#include "btree/locking.h"
#include "btree/read.h"
#include "btree/write.h"
#include "btree/write_buffer.h"

#include "data/checksum.h"
#include "data/compress.h"
#include "data/ec/create.h"
#include "data/move.h"
#include "data/nocow_locking.h"
#include "data/read.h"
#include "data/reconcile/work.h"
#include "data/write.h"

#ifdef CONFIG_DEBUG_FS
#include "debug/async_objs.h"
#endif
#include "debug/debug.h"
#include "debug/sysfs.h"

#include "fs/check.h"
#include "fs/dirent.h"
#include "fs/inode.h"
#include "fs/quota.h"

#include "init/chardev.h"
#include "init/damage.h"
#include "init/dev.h"
#include "init/error.h"
#include "init/recovery.h"
#include "init/passes.h"
#include "init/fs.h"

#include "journal/init.h"
#include "journal/journal.h"
#include "journal/reclaim.h"
#include "journal/seq_blacklist.h"

#include "sb/clean.h"
#include "sb/counters.h"
#include "sb/downgrade.h"
#include "sb/errors.h"
#include "sb/io.h"
#include "sb/members.h"

#include "snapshots/snapshot.h"
#include "snapshots/subvolume.h"

#include "vfs/fs.h"
#include "vfs/io.h"
#include "vfs/buffered.h"
#include "vfs/direct.h"

#include "util/enumerated_ref.h"
#include "init/registry_gen.h"
#include "util/futex_gen.h"
#ifdef __KERNEL__
#include "util/thread_with_file_gen.h"
#endif

#include <linux/backing-dev.h>
#include <linux/blkdev.h>
#include <linux/debugfs.h>
#include <linux/device.h>
#include <linux/idr.h>
#include <linux/module.h>
#include <linux/percpu.h>
#include <linux/random.h>
#include <linux/sysfs.h>

MODULE_LICENSE("GPL");
MODULE_AUTHOR("Kent Overstreet <kent.overstreet@gmail.com>");
MODULE_DESCRIPTION("bcachefs filesystem");

#define x(n)		#n,
const char * const bch2_fs_flag_strs[] = {
	BCH_FS_FLAGS()
	NULL
};

const char * const bch2_write_refs[] = {
	BCH_WRITE_REFS()
	NULL
};
#undef x

static int kern_soh_to_loglevel(const char *fmt)
{
	if (fmt[0] == KERN_SOH[0] &&
	    fmt[1] >= '0' && fmt[1] <= '9')
		return fmt[1] - '0';
	else
		return -1;
}

static unsigned loglevel_opt(struct bch_fs *c)
{
	return c->loglevel ?: c->opts.verbose ? 7: 6;
}

__printf(2, 0)
static void bch2_print_maybe_redirect(struct stdio_redirect *stdio, const char *fmt, va_list args)
{
#ifdef __KERNEL__
	if (unlikely(stdio)) {
		if (fmt[0] == KERN_SOH[0])
			fmt += 2;

		/* We may be in atomic context, and nonblocking drops what won't fit */
		CLASS(printbuf, buf)();
		printbuf_atomic_inc(&buf);
		prt_vprintf(&buf, fmt, args);
		if (!buf.allocation_failure)
			bch2_stdio_redirect_write(stdio, true, buf.buf, buf.pos);
		return;
	}
#endif
	vprintk(fmt, args);
}

void __bch2_print(struct bch_fs *c, const char *fmt, ...)
{
	int loglevel = kern_soh_to_loglevel(fmt);
	if (loglevel < 0)
		loglevel = c->prev_loglevel;
	else
		c->prev_loglevel = loglevel;

	if (loglevel > loglevel_opt(c))
		return;

#ifndef __KERNEL__
	if (fmt[0] == KERN_SOH[0])
		fmt += 2;
#endif

	struct stdio_redirect *stdio = bch2_fs_stdio_redirect_log(c);

	va_list args;
	va_start(args, fmt);
	bch2_print_maybe_redirect(stdio, fmt, args);
	va_end(args);
}

/*
 * For Rust's ratelimited log macros, which keep a ratelimit_state per call
 * site as bch2_ratelimit() does, but have to initialize it at runtime:
 */
void bch2_ratelimit_state_init(struct ratelimit_state *rs)
{
	ratelimit_default_init(rs);
}

/* __bch2_ratelimit(), with the caller's function name for the "suppressed" message: */
bool bch2_ratelimit_suppress(struct bch_fs *c, struct ratelimit_state *rs, const char *func)
{
	return c->opts.ratelimit_errors && !___ratelimit(rs, func);
}

/*
 * For Rust's warn_on!() in the kernel build: the kernel's own warn_on!() pulls
 * the arch's WARN asm from a file generated in the kernel's build tree
 * (rust/kernel/generated_arch_warn_asm.rs), which distro headers packages
 * don't ship, so a DKMS build against them fails:
 */
void bch2_rust_warn(const char *file, unsigned line)
{
	WARN(1, "WARNING at %s:%u\n", file, line);
}

static struct attribute *bcachefs_files[] = {
	NULL
};

static void bcachefs_release(struct kobject *k)
{
}

static const struct sysfs_ops bcachefs_sysfs_ops = {};

KTYPE(bcachefs);

struct kobject bcachefs_kobj;

/* Global interfaces/init */

static void bcachefs_exit(void)
{
	bch2_debug_exit();
	bch2_vfs_exit();
	bch2_chardev_exit();
	bch2_btree_key_cache_exit();
	bch2_lock_graph_exit();
	kobject_put(&bcachefs_kobj);
}

static int __init bcachefs_init(void)
{
	bch2_bkey_pack_test();

	bch2_futex_init();
	bch2_dirent_init();
	bch2_fs_registry_init();

	kobject_init(&bcachefs_kobj, &bcachefs_ktype);

	if (kobject_add(&bcachefs_kobj, fs_kobj, "bcachefs") ||
	    bch2_lock_graph_init() ||
	    bch2_btree_key_cache_init() ||
	    bch2_chardev_init() ||
	    bch2_vfs_init() ||
	    bch2_debug_init())
		goto err;

	return 0;
err:
	bcachefs_exit();
	return -ENOMEM;
}

__maybe_unused
static unsigned bch2_metadata_version = bcachefs_metadata_version_current;
module_param_named(version, bch2_metadata_version, uint, 0444);

module_exit(bcachefs_exit);
module_init(bcachefs_init);
