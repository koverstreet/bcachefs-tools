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

static bool bch2_fs_will_resize_on_mount(struct bch_fs *);

/* Filesystem startup/shutdown: */

static bool check_version_upgrade(struct bch_fs *c, struct printbuf *out)
{
	unsigned latest_version	= bcachefs_metadata_version_current;
	unsigned latest_compatible = min(latest_version,
					 bch2_latest_compatible_version(c->sb.version));
	unsigned old_version = c->sb.version_upgrade_complete ?: c->sb.version;
	unsigned new_version = 0;
	bool ret = false;

	if (old_version < bcachefs_metadata_required_upgrade_below) {
		if (c->opts.version_upgrade == BCH_VERSION_UPGRADE_incompatible ||
		    latest_compatible < bcachefs_metadata_required_upgrade_below)
			new_version = latest_version;
		else
			new_version = latest_compatible;
	} else {
		switch (c->opts.version_upgrade) {
		case BCH_VERSION_UPGRADE_compatible:
			new_version = latest_compatible;
			break;
		case BCH_VERSION_UPGRADE_incompatible:
			new_version = latest_version;
			break;
		case BCH_VERSION_UPGRADE_none:
			new_version = min(old_version, latest_version);
			break;
		}
	}

	if (new_version > old_version) {
		if (old_version < bcachefs_metadata_required_upgrade_below)
			prt_str(out, "Version upgrade required:\n");

		if (old_version != c->sb.version) {
			prt_str(out, "Version upgrade from ");
			bch2_version_to_text(out, c->sb.version_upgrade_complete);
			prt_str(out, " to ");
			bch2_version_to_text(out, c->sb.version);
			prt_str(out, " incomplete\n");
		}

		prt_printf(out, "Doing %s version upgrade from ",
			   BCH_VERSION_MAJOR(old_version) != BCH_VERSION_MAJOR(new_version)
			   ? "incompatible" : "compatible");
		bch2_version_to_text(out, old_version);
		prt_str(out, " to ");
		bch2_version_to_text(out, new_version);
		prt_newline(out);

		struct bch_sb_field_ext *ext = bch2_sb_field_get(c->disk_sb.sb, ext);
		__le64 passes = ext->recovery_passes_required[0];
		bch2_sb_set_upgrade(c, old_version, new_version);
		passes = ext->recovery_passes_required[0] & ~passes;

		if (passes) {
			prt_str(out, "Upgrade requires recovery passes: ");
			prt_bitflags(out, bch2_recovery_passes,
				     bch2_recovery_passes_from_stable(le64_to_cpu(passes)));
			prt_newline(out);
		}

		ret = true;
	}

	if (new_version > c->sb.version_incompat_allowed &&
	    c->opts.version_upgrade == BCH_VERSION_UPGRADE_incompatible) {
		prt_str(out, "Now allowing incompatible features up to ");
		bch2_version_to_text(out, new_version);
		prt_str(out, ", previously allowed up to ");
		bch2_version_to_text(out, c->sb.version_incompat_allowed);
		prt_newline(out);

		ret = true;
	}

	if (ret)
		bch2_sb_upgrade(c, new_version,
				c->opts.version_upgrade == BCH_VERSION_UPGRADE_incompatible);

	return ret;
}

/*
 * Prepare the superblock for a start: last_mount, required recovery passes,
 * version upgrade/downgrade, and what those imply. An open that will never
 * start - offline superblock edits - skips this, so its in-memory superblock
 * stays what's on disk plus the caller's edit, and writing it persists only
 * that. See __bch2_write_super().
 */
static void bch2_fs_sb_prep_for_start(struct bch_fs *c, struct printbuf *out)
{
	scoped_guard(mutex_noio, &c->sb_lock) {
		struct bch_sb_field_ext *ext = bch2_sb_field_get(c->disk_sb.sb, ext);

		__le64 now = cpu_to_le64(ktime_get_real_seconds());
		scoped_guard(rcu)
			for_each_online_member_rcu(c, ca)
				bch2_members_v2_get_mut(c->disk_sb.sb, ca->dev_idx)->last_mount = now;

		if (BCH_SB_HAS_TOPOLOGY_ERRORS(c->disk_sb.sb))
			ext->recovery_passes_required[0] |=
				cpu_to_le64(bch2_recovery_passes_to_stable(BIT_ULL(BCH_RECOVERY_PASS_check_topology)));

		u64 sb_passes = bch2_recovery_passes_from_stable(le64_to_cpu(ext->recovery_passes_required[0]));
		if (sb_passes) {
			prt_str(out, "superblock requires following recovery passes to be run: ");
			prt_bitflags(out, bch2_recovery_passes, sb_passes);
			prt_newline(out);
		}

		u64 btrees_lost_data = le64_to_cpu(ext->btrees_lost_data);
		if (btrees_lost_data) {
			prt_str(out, "superblock indicates damage to following btrees:  ");
			prt_bitflags(out, __bch2_btree_ids, btrees_lost_data);
			prt_newline(out);
		}

		if (test_bit(BCH_FS_may_upgrade_downgrade, &c->flags)) {
			if (bch2_check_version_downgrade(c)) {
				prt_str(out, "Version downgrade required");

				__le64 passes = ext->recovery_passes_required[0];
				bch2_sb_set_downgrade(c,
						      BCH_VERSION_MINOR(bcachefs_metadata_version_current),
						      BCH_VERSION_MINOR(c->sb.version));
				passes = ext->recovery_passes_required[0] & ~passes;
				if (passes) {
					prt_str(out, ", running recovery passes: ");
					prt_bitflags(out, bch2_recovery_passes,
						     bch2_recovery_passes_from_stable(le64_to_cpu(passes)));
				}
				prt_newline(out);
			}

			check_version_upgrade(c, out);
		}

		c->opts.recovery_passes |= bch2_recovery_passes_from_stable(le64_to_cpu(ext->recovery_passes_required[0]));

		if (c->sb.version_upgrade_complete < bcachefs_metadata_version_autofix_errors)
			SET_BCH_SB_ERROR_ACTION(c->disk_sb.sb, BCH_ON_ERROR_fix_safe);

		unsigned extent_bp_shift_needed = ilog2(c->opts.encoded_extent_max >> 9) + 1;
		if (extent_bp_shift_needed > c->sb.extent_bp_shift) {
			prt_printf(out, "extent_bp_shift too small: must repair backpointers\n");
			SET_BCH_SB_EXTENT_BP_SHIFT(c->disk_sb.sb, extent_bp_shift_needed);
			ext->recovery_passes_required[0] |=
				cpu_to_le64(bch2_recovery_passes_to_stable(BIT_ULL(BCH_RECOVERY_PASS_check_extents_to_backpointers)));
			ext->recovery_passes_required[0] |=
				cpu_to_le64(bch2_recovery_passes_to_stable(BIT_ULL(BCH_RECOVERY_PASS_check_backpointers_to_extents)));
			__set_bit_le64(BCH_FSCK_ERR_backpointer_to_missing_ptr, ext->errors_silent);
			__set_bit_le64(BCH_FSCK_ERR_ptr_to_missing_backpointer, ext->errors_silent);
		}

		set_bit(BCH_FS_sb_dirty, &c->flags);
	}
}

noinline_for_stack
int bch2_fs_opt_version_init(struct bch_fs *c, struct printbuf *out)
{
	if (c->opts.norecovery) {
		c->opts.recovery_pass_last = c->opts.recovery_pass_last
			? min(c->opts.recovery_pass_last, BCH_RECOVERY_PASS_snapshots_read)
			: BCH_RECOVERY_PASS_snapshots_read;
		c->opts.nochanges = true;
	}

	if (c->opts.nochanges)
		c->opts.read_only = true;

	if (c->opts.will_not_start)
		c->opts.nostart = true;

	if (c->opts.journal_rewind)
		c->opts.fsck = true;

	if (!(c->sb.features & (BIT_ULL(BCH_FEATURE_small_image)|
			        BIT_ULL(BCH_FEATURE_no_default_sb))) ||
	    bch2_fs_will_resize_on_mount(c))
		set_bit(BCH_FS_may_upgrade_downgrade, &c->flags);

	prt_str(out, "starting version ");
	bch2_version_to_text(out, c->sb.version);
	prt_newline(out);

	CLASS(printbuf, opts)();
	bch2_opts_to_text(&opts, c->opts, c, c->disk_sb.sb, NULL,
			  OPT_MOUNT, 0, OPT_SHOW_MOUNT_STYLE);
	if (opts.pos)
		prt_printf(out, "with options: %s\n", opts.buf);

	if (c->sb.version_incompat_allowed != c->sb.version) {
		prt_printf(out, "allowing incompatible features up to ");
		bch2_version_to_text(out, c->sb.version_incompat_allowed);
		prt_newline(out);
	}

	if (c->opts.verbose) {
		prt_printf(out, "features: ");
		prt_bitflags(out, bch2_sb_features, c->sb.features);
		prt_newline(out);
	}

	if (c->sb.multi_device) {
		bool first = true;
		prt_printf(out, "with devices: ");
		for_each_online_member(c, ca, BCH_DEV_READ_REF_bch2_online_devs) {
			if (!first)
				prt_char(out, ',');
			first = false;
			prt_str(out, ca->name);
		}
		prt_newline(out);
	}

	/* cf_encoding log message should be here, but it breaks xfstests - sigh */

	if (c->opts.journal_rewind)
		prt_printf(out, "rewinding journal, fsck required\n");

	if (!c->opts.will_not_start)
		bch2_fs_sb_prep_for_start(c, out);

	if (c->sb.clean)
		set_bit(BCH_FS_clean_recovery, &c->flags);
	if (c->opts.fsck)
		set_bit(BCH_FS_in_fsck, &c->flags);
	set_bit(BCH_FS_in_recovery, &c->flags);

	/* this really should be part of our one multi line mount message, but -
	 * xfstests... */
	if (c->cf_encoding)
		bch_info(c, "Using encoding defined by superblock: utf8-%u.%u.%u",
			   unicode_major(BCH_FS_DEFAULT_UTF8_ENCODING),
			   unicode_minor(BCH_FS_DEFAULT_UTF8_ENCODING),
			   unicode_rev(BCH_FS_DEFAULT_UTF8_ENCODING));

	if (BCH_SB_INITIALIZED(c->disk_sb.sb)) {
		if (!(c->sb.features & BIT_ULL(BCH_FEATURE_new_extent_overwrite))) {
			prt_str(out, "feature new_extent_overwrite not set, filesystem no longer supported\n");
			return bch_err_throw(c, EINVAL_missing_new_extent_overwrite);
		}

		if (c->sb.version_min < bcachefs_metadata_version_btree_ptr_sectors_written) {
			prt_str(out, "version_min < version_btree_ptr_sectors_written\n");
			prt_str(out, "filesystem needs upgrade from older version; run fsck from older bcachefs-tools to fix\n");
			return bch_err_throw(c, EINVAL_version_min_too_old);
		}
	}

	bch2_fs_mi_field_upgrades(c);

	return 0;
}

static bool bch2_dev_will_resize_on_mount(struct bch_dev *ca)
{
	return ca->mi.resize_on_mount &&
		ca->mi.nbuckets < div64_u64(get_capacity(ca->disk_sb.bdev->bd_disk),
					    ca->mi.bucket_size);
}

static bool bch2_fs_will_resize_on_mount(struct bch_fs *c)
{
	bool ret = false;
	for_each_online_member(c, ca, BCH_DEV_READ_REF_fs_resize_on_mount)
		ret |= bch2_dev_will_resize_on_mount(ca);
	return ret;
}

int bch2_fs_resize_on_mount(struct bch_fs *c)
{
	for_each_online_member(c, ca, BCH_DEV_READ_REF_fs_resize_on_mount) {
		if (bch2_dev_will_resize_on_mount(ca)) {
			u64 old_nbuckets = ca->mi.nbuckets;
			u64 new_nbuckets = div64_u64(get_capacity(ca->disk_sb.bdev->bd_disk),
						     ca->mi.bucket_size);

			bch_info_dev(ca, "resizing to size %llu", new_nbuckets * ca->mi.bucket_size);
			int ret = bch2_dev_buckets_resize(c, ca, new_nbuckets);
			bch_err_fn_dev(ca, ret);
			if (ret) {
				enumerated_ref_put(&ca->io_ref[READ],
						   BCH_DEV_READ_REF_fs_resize_on_mount);
				return ret;
			}

			scoped_guard(mutex_noio, &c->sb_lock) {
				struct bch_member *m =
					bch2_members_v2_get_mut(c->disk_sb.sb, ca->dev_idx);
				m->nbuckets = cpu_to_le64(new_nbuckets);
				SET_BCH_MEMBER_RESIZE_ON_MOUNT(m, false);

				c->disk_sb.sb->features[0] &= ~cpu_to_le64(BIT_ULL(BCH_FEATURE_small_image));
				bch2_write_super(c);
			}

			if (ca->mi.freespace_initialized) {
				ret = __bch2_dev_resize_alloc(ca, old_nbuckets, new_nbuckets);
				if (ret) {
					enumerated_ref_put(&ca->io_ref[READ],
							BCH_DEV_READ_REF_fs_resize_on_mount);
					return ret;
				}
			}
		}
	}
	return 0;
}

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

#define BCH_DEBUG_PARAM(name, description) DEFINE_STATIC_KEY_FALSE(bch2_##name);
BCH_DEBUG_PARAMS_ALL()
#undef BCH_DEBUG_PARAM

static int bch2_param_set_static_key_t(const char *val, const struct kernel_param *kp)
{
	/* Match bool exactly, by re-using it. */
	struct static_key *key = kp->arg;
	struct kernel_param boolkp = *kp;
	bool v;
	int ret;

	boolkp.arg = &v;

	ret = param_set_bool(val, &boolkp);
	if (ret)
		return ret;
	if (v)
		static_key_enable(key);
	else
		static_key_disable(key);
	return 0;
}

static int bch2_param_get_static_key_t(char *buffer, const struct kernel_param *kp)
{
	struct static_key *key = kp->arg;
	return sprintf(buffer, "%c\n", static_key_enabled(key) ? 'Y' : 'N');
}

/* this is unused in userspace - silence the warning */
__maybe_unused
static const struct kernel_param_ops bch2_param_ops_static_key_t = {
	.flags = KERNEL_PARAM_OPS_FL_NOARG,
	.set = bch2_param_set_static_key_t,
	.get = bch2_param_get_static_key_t,
};

#define BCH_DEBUG_PARAM(name, description)				\
	module_param_cb(name, &bch2_param_ops_static_key_t, &bch2_##name.key, 0644);\
	__MODULE_PARM_TYPE(name, "static_key_t");			\
	MODULE_PARM_DESC(name, description);
BCH_DEBUG_PARAMS()
#undef BCH_DEBUG_PARAM

__maybe_unused
static unsigned bch2_metadata_version = bcachefs_metadata_version_current;
module_param_named(version, bch2_metadata_version, uint, 0444);

module_exit(bcachefs_exit);
module_init(bcachefs_init);
