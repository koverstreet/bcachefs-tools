// SPDX-License-Identifier: GPL-2.0
/*
 * bcache sysfs interfaces
 *
 * Copyright 2010, 2011 Kent Overstreet <kent.overstreet@gmail.com>
 * Copyright 2012 Google, Inc.
 */

#include "bcachefs.h"

#include "alloc/accounting.h"
#include "alloc/background.h"
#include "alloc/buckets.h"
#include "alloc/discard.h"
#include "alloc/disk_groups.h"
#include "alloc/foreground.h"
#include "alloc/replicas.h"

#include "btree/cache.h"
#include "btree/check.h"
#include "btree/interior.h"
#include "btree/iter.h"
#include "btree/key_cache.h"
#include "btree/read.h"
#include "btree/update.h"
#include "btree/write.h"
#include "btree/write_buffer.h"

#include "data/compress.h"
#include "data/copygc.h"
#include "data/ec/create.h"
#include "data/move.h"
#include "data/nocow_locking.h"
#include "data/reconcile/work.h"

#include "debug/sysfs.h"

#include "fs/dirent.h"
#include "fs/inode.h"
#include "fs/logged_ops.h"

#include "init/error.h"
#include "init/fs.h"
#include "init/passes.h"

#include "journal/journal.h"
#include "journal/reclaim.h"
#include "journal/write.h"

#include "sb/counters.h"
#include "sb/errors.h"
#include "sb/io.h"

#include "util/clock.h"
#include "util/enumerated_ref.h"
#include "util/util.h"

#include <linux/blkdev.h>
#include <linux/sort.h>
#include <linux/sched/clock.h>
#include <linux/sysfs.h>


#define SYSFS_OPS(type)							\
const struct sysfs_ops type ## _sysfs_ops = {				\
	.show	= type ## _show,					\
	.store	= type ## _store					\
}

#define SHOW(fn)							\
static ssize_t fn ## _to_text(struct printbuf *,			\
			      struct kobject *, struct attribute *);	\
									\
static ssize_t fn ## _show(struct kobject *kobj, struct attribute *attr,\
			   char *buf)					\
{									\
	CLASS(printbuf, out)();						\
	ssize_t ret = fn ## _to_text(&out, kobj, attr);			\
									\
	bch2_printbuf_ensure_trailing_newline(&out);			\
									\
	if (!ret && out.allocation_failure)				\
		ret = -ENOMEM;						\
									\
	if (!ret) {							\
		ret = min_t(size_t, out.pos, PAGE_SIZE - 1);		\
		memcpy(buf, out.buf, ret);				\
	}								\
	return bch2_err_class(ret);					\
}									\
									\
static ssize_t fn ## _to_text(struct printbuf *out, struct kobject *kobj,\
			      struct attribute *attr)

#define STORE(fn)							\
static ssize_t fn ## _store_inner(struct kobject *, struct attribute *,\
			    const char *, size_t);			\
									\
static ssize_t fn ## _store(struct kobject *kobj, struct attribute *attr,\
			    const char *buf, size_t size)		\
{									\
	return bch2_err_class(fn##_store_inner(kobj, attr, buf, size));	\
}									\
									\
static ssize_t fn ## _store_inner(struct kobject *kobj, struct attribute *attr,\
				  const char *buf, size_t size)

#define __sysfs_attribute(_name, _mode)					\
	static struct attribute sysfs_##_name =				\
		{ .name = #_name, .mode = _mode }

#define write_attribute(n)	__sysfs_attribute(n, 0200)
#define read_attribute(n)	__sysfs_attribute(n, 0444)
#define rw_attribute(n)		__sysfs_attribute(n, 0644)

#define sysfs_printf(file, fmt, ...)					\
do {									\
	if (attr == &sysfs_ ## file)					\
		prt_printf(out, fmt "\n", __VA_ARGS__);			\
} while (0)

#define sysfs_print(file, var)						\
do {									\
	if (attr == &sysfs_ ## file)					\
		snprint(out, var);					\
} while (0)

#define sysfs_strtoul(file, var)					\
do {									\
	if (attr == &sysfs_ ## file)					\
		return strtoul_safe(buf, var) ?: (ssize_t) size;	\
} while (0)

#define sysfs_strtoul_clamp(file, var, min, max)			\
do {									\
	if (attr == &sysfs_ ## file)					\
		return strtoul_safe_clamp(buf, var, min, max)		\
			?: (ssize_t) size;				\
} while (0)

read_attribute(uuid);
read_attribute(first_bucket);
read_attribute(nbuckets);
read_attribute(io_done);
read_attribute(io_errors);
write_attribute(io_errors_reset);

read_attribute(io_latency_read);
read_attribute(io_latency_write);
read_attribute(io_latency_stats_read);
read_attribute(io_latency_stats_write);
read_attribute(io_latency_stats_read_json);
read_attribute(io_latency_stats_write_json);
#ifndef CONFIG_BCACHEFS_NO_LATENCY_ACCT
read_attribute(congested);
#endif

read_attribute(open_buckets);
read_attribute(discards);

read_attribute(read_refs);
read_attribute(write_refs);

read_attribute(has_data);
read_attribute(alloc_debug);

/* options */

static ssize_t sysfs_opt_show(struct bch_fs *c,
			      struct bch_dev *ca,
			      enum bch_opt_id id,
			      struct printbuf *out)
{
	const struct bch_option *opt = bch2_opt_table + id;
	u64 v;

	/*
	 * c->disk_sb.sb is reallocated and freed under sb_lock, so hold it
	 * across the reads below - previously only the STR_MEMBER branch
	 * did, and the other paths read a superblock that a concurrent
	 * device add/remove could free.
	 */
	guard(mutex_noio)(&c->sb_lock);

	if (ca) {
		if (opt->type == BCH_OPT_STR_MEMBER) {
			/* The value lives in the member, not a u64 - render it here: */
			struct bch_member m = bch2_sb_member_get(c->disk_sb.sb, ca->dev_idx);
			prt_printf(out, "%.*s\n", (int) opt->member_size,
				   (char *) &m + opt->member_offset);
			return 0;
		}
		if (!((opt->flags & OPT_DEVICE) && opt->get_member))
			return bch_err_throw(c, EINVAL_sysfs_opt_not_found);
		v = bch2_opt_from_sb(c->disk_sb.sb, id, ca->dev_idx);
	} else {
		if (!(opt->flags & OPT_FS))
			return bch_err_throw(c, EINVAL_sysfs_opt_not_found);
		v = bch2_opt_get_by_id(&c->opts, id);
	}

	bch2_opt_to_text(out, c, c->disk_sb.sb, opt, v, OPT_SHOW_FULL_LIST);
	prt_char(out, '\n');
	return 0;
}

static ssize_t sysfs_opt_store(struct bch_fs *c,
			       struct bch_dev *ca,
			       enum bch_opt_id id,
			       const char *buf, size_t size)
{
	const struct bch_option *opt = bch2_opt_table + id;
	int ret = 0;

	char *tmp __free(kfree) = kstrdup(buf, GFP_KERNEL);
	if (!tmp)
		return -ENOMEM;

	/*
	 * We don't need to take c->writes for correctness, but it eliminates an
	 * unsightly error message in the dmesg log when we're RO:
	 */
	if (unlikely(!enumerated_ref_tryget(&c->writes, BCH_WRITE_REF_sysfs)))
		return -EROFS;

	guard(memalloc_flags)(PF_MEMALLOC_NOIO);
	guard(opt_change_lock)(c);
	CLASS(opt_change_scope, opt_scope)(c);

	char *val = strim(tmp);
	u64 v;
	ret =   bch2_opt_parse(c, opt, val, &v, NULL) ?:
		bch2_opt_hook_pre_set(c, ca, 0, id, v, true, &opt_scope);

	if (!ret) {
		bool is_sb = opt->get_sb || opt->get_member || opt->get_ext ||
			     opt->type == BCH_OPT_STR_MEMBER;
		bool changed = false;

		if (is_sb) {
			changed = bch2_opt_set_sb(c, ca, opt, v, val);
		} else if (!ca) {
			changed = bch2_opt_get_by_id(&c->opts, id) != v;
		} else {
			/* device options that aren't superblock options aren't
			 * supported */
			BUG();
		}

		if (changed) {
			if (!ca) {
				bch2_opt_set_by_id(&c->opts, id, v);
				clear_bit(id, c->mount_opts.d);
			}

			bch2_opt_hook_post_set(c, ca, 0, id, v);
		}

		ret = size;
	}

	enumerated_ref_put(&c->writes, BCH_WRITE_REF_sysfs);
	return ret;
}

SHOW(bch2_fs_opts_dir)
{
	struct bch_fs *c = container_of(kobj, struct bch_fs, opts_dir);
	int id = bch2_opt_lookup(attr->name);
	if (id < 0)
		return 0;

	return sysfs_opt_show(c, NULL, id, out);
}

STORE(bch2_fs_opts_dir)
{
	struct bch_fs *c = container_of(kobj, struct bch_fs, opts_dir);
	int id = bch2_opt_lookup(attr->name);
	if (id < 0)
		return 0;

	return sysfs_opt_store(c, NULL, id, buf, size);
}
SYSFS_OPS(bch2_fs_opts_dir);

struct attribute *bch2_fs_opts_dir_files[] = { NULL };

int bch2_opts_create_sysfs_files(struct kobject *kobj, unsigned type)
{
	for (const struct bch_option *i = bch2_opt_table;
	     i < bch2_opt_table + bch2_opts_nr;
	     i++) {
		if (i->flags & OPT_HIDDEN)
			continue;
		if (!(i->flags & type))
			continue;

		/*
		 * For options that are both OPT_FS and OPT_DEVICE (currently
		 * only @discard), expose only the per-device sysfs entry: the
		 * FS-level entry would alias to c->opts but member-backed
		 * options have no FS-wide superblock field, so writes via the
		 * FS path are silent no-ops and reads can't disambiguate
		 * which scope is being shown. Mount-time @-o discard= remains
		 * the only fs-scope handle.
		 */
		if (type == OPT_FS && (i->flags & OPT_DEVICE))
			continue;

		try(sysfs_create_file(kobj, &i->attr));
	}

	return 0;
}

static const char * const bch2_rw[] = {
	"read",
	"write",
	NULL
};

static __cold void dev_io_done_to_text(struct printbuf *out, struct bch_dev *ca)
{
	prt_printf(out, "{\n");
	for (int rw = 0; rw < 2; rw++) {
		prt_printf(out, "  \"%s\": {\n", bch2_rw[rw]);
		for (int i = 1; i < BCH_DATA_NR; i++)
			prt_printf(out, "    \"%s\": %llu%s\n",
				   bch2_data_type_str(i),
				   percpu_u64_get(&ca->io_done->sectors[rw][i]) << 9,
				   i < BCH_DATA_NR - 1 ? "," : "");
		prt_printf(out, "  }%s\n", rw == 0 ? "," : "");
	}
	prt_printf(out, "}\n");
}

SHOW(bch2_dev)
{
	struct bch_dev *ca = container_of(kobj, struct bch_dev, kobj);
	struct bch_fs *c = ca->fs;

	sysfs_printf(uuid,		"%pU", ca->uuid.b);

	sysfs_print(first_bucket,	ca->mi.first_bucket);
	sysfs_print(nbuckets,		ca->mi.nbuckets);

	if (attr == &sysfs_has_data) {
		prt_bitflags(out, __bch2_data_types, bch2_dev_has_data(c, ca));
		prt_char(out, '\n');
	}

	if (attr == &sysfs_io_done)
		dev_io_done_to_text(out, ca);

	if (attr == &sysfs_io_errors)
		bch2_dev_io_errors_to_text(out, ca);

	sysfs_print(io_latency_read,		atomic64_read(&ca->cur_latency[READ]));
	sysfs_print(io_latency_write,		atomic64_read(&ca->cur_latency[WRITE]));

	if (attr == &sysfs_io_latency_stats_read)
		bch2_time_stats_to_text(out, &ca->io_latency[READ].stats);

	if (attr == &sysfs_io_latency_stats_write)
		bch2_time_stats_to_text(out, &ca->io_latency[WRITE].stats);

	if (attr == &sysfs_io_latency_stats_read_json)
		bch2_time_stats_json_to_text(out, &ca->io_latency[READ].stats, NULL, 0);

	if (attr == &sysfs_io_latency_stats_write_json)
		bch2_time_stats_json_to_text(out, &ca->io_latency[WRITE].stats, NULL, 0);

#ifndef CONFIG_BCACHEFS_NO_LATENCY_ACCT
	if (attr == &sysfs_congested)
		bch2_dev_congested_to_text(out, ca);
#endif

	if (attr == &sysfs_alloc_debug)
		bch2_dev_alloc_debug_to_text(out, ca);

	if (attr == &sysfs_open_buckets)
		bch2_open_buckets_to_text(out, c, ca);

	if (attr == &sysfs_discards)
		bch2_fast_discards_to_text(out, ca);

	int opt_id = bch2_opt_lookup(attr->name);
	if (opt_id >= 0)
		return sysfs_opt_show(c, ca, opt_id, out);

	if (attr == &sysfs_read_refs)
		enumerated_ref_to_text(out, &ca->io_ref[READ], bch2_dev_read_refs);

	if (attr == &sysfs_write_refs)
		enumerated_ref_to_text(out, &ca->io_ref[WRITE], bch2_dev_write_refs);

	return 0;
}

STORE(bch2_dev)
{
	struct bch_dev *ca = container_of(kobj, struct bch_dev, kobj);
	struct bch_fs *c = ca->fs;

	if (attr == &sysfs_io_errors_reset)
		bch2_dev_errors_reset(ca);

	int opt_id = bch2_opt_lookup(attr->name);
	if (opt_id >= 0)
		return sysfs_opt_store(c, ca, opt_id, buf, size);

	return size;
}
SYSFS_OPS(bch2_dev);

struct attribute *bch2_dev_files[] = {
	&sysfs_uuid,
	&sysfs_first_bucket,
	&sysfs_nbuckets,

	&sysfs_has_data,
	&sysfs_io_done,
	&sysfs_io_errors,
	&sysfs_io_errors_reset,

	&sysfs_io_latency_read,
	&sysfs_io_latency_write,
	&sysfs_io_latency_stats_read,
	&sysfs_io_latency_stats_write,
	&sysfs_io_latency_stats_read_json,
	&sysfs_io_latency_stats_write_json,
#ifndef CONFIG_BCACHEFS_NO_LATENCY_ACCT
	&sysfs_congested,
#endif

	/* debug: */
	&sysfs_alloc_debug,
	&sysfs_open_buckets,
	&sysfs_discards,

	&sysfs_read_refs,
	&sysfs_write_refs,
	NULL
};
