#include <getopt.h>
#include <stdio.h>
#include <sys/ioctl.h>

#include <uuid/uuid.h>

#include "linux/sort.h"
#include "linux/rcupdate.h"

#include "bcachefs_ioctl.h"
#include "opts.h"
#include "alloc/buckets.h"
#include "alloc/accounting.h"
#include "sb/io.h"
#include "util/darray.h"

#include "cmds.h"
#include "libbcachefs.h"

#define FS_USAGE_FIELDS()		\
	x(replicas)			\
	x(btree)			\
	x(compression)			\
	x(rebalance_work)		\
	x(devices)

enum __fs_usage_fields {
#define x(n)		__FS_USAGE_##n,
	FS_USAGE_FIELDS()
#undef x
};

enum fs_usage_fields {
#define x(n)		FS_USAGE_##n = BIT(__FS_USAGE_##n),
	FS_USAGE_FIELDS()
#undef x
};

const char * const fs_usage_field_strs[] = {
#define x(n)		[__FS_USAGE_##n] = #n,
	FS_USAGE_FIELDS()
#undef x
	NULL
};

struct dev_usage {
	unsigned			idx;
	const char			*label;
	const char			*dev;
	struct bch_ioctl_dev_usage_v2	*u;
	u64				leaving;
};

static void dev_usage_to_text(struct printbuf *out,
			      struct dev_usage *d,
			      bool full)
{
	struct bch_ioctl_dev_usage_v2 *u = d->u;

	u64 used = 0, capacity = u->nr_buckets * u->bucket_size;
	for (unsigned type = 0; type < u->nr_data_types; type++)
		if (type != BCH_DATA_unstriped)
			used += u->d[type].sectors;

	prt_printf(out, "%s (device %u):\t%s\t%s\t%02u%%\n",
		   d->label ?: "(no label)", d->idx,
		   d->dev ?: "(device not found)",
		   bch2_member_states[u->state],
		   (unsigned) (used * 100 / capacity));

	printbuf_indent_add(out, 2);
	prt_printf(out, "\tdata\tbuckets\tfragmented\n");

	for (unsigned type = 0; type < u->nr_data_types; type++) {
		bch2_prt_data_type(out, type);
		prt_printf(out, ":\t");

		/* sectors are 0 for empty bucket data types, so calculate sectors for them */
		u64 sectors = data_type_is_empty(type)
			? u->d[type].buckets * u->bucket_size
			: u->d[type].sectors;
		prt_units_u64(out, sectors << 9);

		prt_printf(out, "\t%llu\t", u->d[type].buckets);

		u64 fragmented = u->d[type].buckets * u->bucket_size - sectors;
		if (fragmented)
			prt_units_u64(out, fragmented << 9);
		prt_newline(out);
	}

	prt_printf(out, "capacity:\t");
	prt_units_u64(out, (u->nr_buckets * u->bucket_size) << 9);
	prt_printf(out, "\t%llu\n", u->nr_buckets);

	prt_printf(out, "bucket size:\t");
	prt_units_u64(out, u->bucket_size << 9);
	prt_newline(out);

	printbuf_indent_sub(out, 2);
	prt_newline(out);

	free(u);
}

static int dev_by_label_cmp(const void *_l, const void *_r)
{
	const struct dev_name *l = _l, *r = _r;

	return  (l->label && r->label
		 ? strcmp(l->label, r->label) : 0) ?:
		(l->dev && r->dev
		 ? strcmp(l->dev, r->dev) : 0) ?:
		cmp_int(l->idx, r->idx);
}

static u64 dev_leaving_dev(struct bch_ioctl_query_accounting *in, unsigned dev)
{
	for (struct bkey_i_accounting *a = in->accounting;
	     a < (struct bkey_i_accounting *) ((u64 *) in->accounting + in->accounting_u64s);
	     a = bkey_i_to_accounting(bkey_next(&a->k_i))) {
		struct disk_accounting_pos acc_k;
		bpos_to_disk_accounting_pos(&acc_k, a->k.p);

		if (acc_k.dev_leaving.dev == dev)
			return a->v.d[0];
	}

	return 0;
}

static void devs_usage_to_text(struct printbuf *out,
			       struct bchfs_handle fs,
			       dev_names dev_names,
			       bool full)
{
	sort(dev_names.data, dev_names.nr,
	     sizeof(dev_names.data[0]), dev_by_label_cmp, NULL);

	struct bch_ioctl_query_accounting *dev_leaving =
		bchu_fs_accounting(fs, BIT(BCH_DISK_ACCOUNTING_dev_leaving));

	DARRAY(struct dev_usage) dev_usage = {};

	darray_for_each(dev_names, i)
		darray_push(&dev_usage, ((struct dev_usage) {
			.idx		= i->idx,
			.label		= i->label,
			.dev		= i->dev,
			.u		= bchu_dev_usage(fs, i->idx),
			.leaving	= dev_leaving_dev(dev_leaving, i->idx),
		}));

	bool print_dev_leaving = false;
	darray_for_each(dev_usage, i)
		print_dev_leaving |= i->leaving != 0;

	printbuf_tabstops_reset(out);
	prt_newline(out);

	if (full) {
		printbuf_tabstop_start(out);

		darray_for_each(dev_usage, d)
			dev_usage_to_text(out, d, full);

		printbuf_tabstop_finish(out);
	} else {
		printbuf_tabstop_start(out);

		prt_printf(out, "Device label\tDevice\tState\tSize\tUsed\tUse%%\t");
		if (print_dev_leaving)
			prt_printf(out, "Leaving\t");
		prt_newline(out);

		darray_for_each(dev_usage, d) {
			struct bch_ioctl_dev_usage_v2 *u = d->u;

			u64 used = 0, capacity = u->nr_buckets * u->bucket_size;
			for (unsigned type = 0; type < u->nr_data_types; type++)
				if (type != BCH_DATA_unstriped)
					used += u->d[type].sectors;

			prt_printf(out, "%s (device %u):\t%s\t%s\t",
				   d->label ?: "(no label)", d->idx,
				   d->dev ?: "(device not found)",
				   bch2_member_states[u->state]);

			prt_units_u64(out, (u->nr_buckets * u->bucket_size) << 9);
			prt_printf(out, "\t");
			prt_units_u64(out, used << 9);
			prt_printf(out, "\t");

			prt_printf(out, "%02u%%\t", (unsigned) (used * 100 / capacity));

			if (d->leaving) {
				prt_units_u64(out, d->leaving << 9);
				prt_printf(out, "\t");
			}

			prt_newline(out);
		}
		printbuf_tabstop_finish(out);
	}
}

static void persistent_reserved_to_text(struct printbuf *out,
					unsigned nr_replicas, s64 sectors)
{
	if (!sectors)
		return;

	prt_printf(out, "reserved:\t%u/%u\t[] ", 1, nr_replicas);
	prt_units_u64(out, sectors << 9);
	prt_newline(out);
}

struct durability_x_degraded {
	unsigned	durability;
	unsigned	minus_degraded;
};

static struct durability_x_degraded replicas_durability(const struct bch_replicas_entry_v1 *r,
							dev_names *dev_names)
{
	struct durability_x_degraded ret = {};
	unsigned degraded = 0;

	for (unsigned i = 0; i < r->nr_devs; i++) {
		unsigned dev_idx = r->devs[i];
		struct dev_name *dev = dev_idx_to_name(dev_names, dev_idx);

		unsigned durability = dev ? dev->durability : 1;

		if (!dev || !dev->dev || dev->state == BCH_MEMBER_STATE_evacuating)
			degraded += durability;
		ret.durability += durability;
	}

	if (r->nr_required > 1)
		ret.durability = r->nr_devs - r->nr_required + 1;

	ret.minus_degraded = max_t(int, 0, ret.durability - degraded);

	return ret;
}

static void replicas_usage_to_text(struct printbuf *out,
				   const struct bch_replicas_entry_v1 *r,
				   s64 sectors,
				   dev_names *dev_names)
{
	if (!sectors)
		return;

	struct durability_x_degraded durability = replicas_durability(r, dev_names);

	bch2_prt_data_type(out, r->data_type);
	prt_printf(out, ":\t%u/%u\t%u\t[",
		   r->nr_required, r->nr_devs,
		   durability.durability);

	for (unsigned i = 0; i < r->nr_devs; i++) {
		unsigned dev_idx = r->devs[i];
		struct dev_name *dev = dev_idx_to_name(dev_names, dev_idx);

		if (i)
			prt_char(out, ' ');
		if (dev && dev->dev)
			prt_str(out, dev->dev);
		else if (dev_idx == BCH_SB_MEMBER_INVALID)
			prt_str(out, "none");
		else
			prt_printf(out, "%u", dev_idx);
	}
	prt_printf(out, "]\t");

	prt_units_u64(out, sectors << 9);
	prt_newline(out);
}

#define for_each_usage_replica(_u, _r)					\
	for (_r = (_u)->replicas;					\
	     _r != (void *) (_u)->replicas + (_u)->replica_entries_bytes;\
	     _r = replicas_usage_next(_r),				\
	     BUG_ON((void *) _r > (void *) (_u)->replicas + (_u)->replica_entries_bytes))

typedef DARRAY(struct bkey_i_accounting *) darray_accounting_p;

static int accounting_p_cmp(const void *_l, const void *_r)
{
	const struct bkey_i_accounting * const *l = _l;
	const struct bkey_i_accounting * const *r = _r;

	struct bpos lp = (*l)->k.p, rp = (*r)->k.p;

	return bpos_cmp(lp, rp);
}

static void accounting_sort(darray_accounting_p *sorted,
			    struct bch_ioctl_query_accounting *in)
{
	for (struct bkey_i_accounting *a = in->accounting;
	     a < (struct bkey_i_accounting *) ((u64 *) in->accounting + in->accounting_u64s);
	     a = bkey_i_to_accounting(bkey_next(&a->k_i)))
		if (darray_push(sorted, a))
			die("memory allocation failure");

	sort(sorted->data, sorted->nr, sizeof(sorted->data[0]), accounting_p_cmp, NULL);
}

static void accounting_swab_if_old(struct bch_ioctl_query_accounting *in)
{
	unsigned kernel_version = bcachefs_kernel_version();

	if (kernel_version &&
	    kernel_version < bcachefs_metadata_version_disk_accounting_big_endian)
		for (struct bkey_i_accounting *a = in->accounting;
		     a < (struct bkey_i_accounting *) ((u64 *) in->accounting + in->accounting_u64s);
		     a = bkey_i_to_accounting(bkey_next(&a->k_i)))
			bch2_bpos_swab(&a->k.p);
}

static void replicas_summary_to_text(struct printbuf *out,
				     darray_accounting_p accounting,
				     dev_names dev_names)
{
	DARRAY(darray_u64) replicas_x_degraded = {};
	u64 cached = 0, reserved = 0;

	/* XXX split out metadata, erasure coded */

	/* summarize replicas - 1x replicated, 2x replicated, degraded... */
	darray_for_each(accounting, i) {
		struct bkey_i_accounting *a = *i;

		struct disk_accounting_pos acc_k;
		bpos_to_disk_accounting_pos(&acc_k, a->k.p);

		if (acc_k.type == BCH_DISK_ACCOUNTING_persistent_reserved) {
			reserved += a->v.d[0];
			continue;
		}

		if (acc_k.type != BCH_DISK_ACCOUNTING_replicas)
			continue;

		if (acc_k.replicas.data_type == BCH_DATA_cached) {
			cached += a->v.d[0];
			continue;
		}

		struct durability_x_degraded d = replicas_durability(&acc_k.replicas, &dev_names);
		unsigned degraded = d.durability - d.minus_degraded;

		while (replicas_x_degraded.nr <= d.durability)
			darray_push(&replicas_x_degraded, (darray_u64) {});

		while (replicas_x_degraded.data[d.durability].nr <= degraded)
			darray_push(&replicas_x_degraded.data[d.durability], 0);

		replicas_x_degraded.data[d.durability].data[degraded] += a->v.d[0];
	}

	prt_printf(out, "\nData by durability desired and amount degraded:\n");

	unsigned max_degraded = 0;
	darray_for_each(replicas_x_degraded, i)
		max_degraded = max(max_degraded, i->nr);

	printbuf_tabstops_reset(out);
	printbuf_tabstop_start(out);

	prt_printf(out, "\t");
	for (unsigned i = 0; i < max_degraded; i++) {
		if (!i)
			prt_printf(out, "undegraded\t");
		else
			prt_printf(out, "-%ux\t", i);
	}
	prt_newline(out);

	darray_for_each(replicas_x_degraded, i) {
		if (!i->nr)
			continue;

		prt_printf(out, "%zux:\t", i - replicas_x_degraded.data);

		darray_for_each(*i, j) {
			if (*j)
				prt_units_u64(out, *j << 9);
			prt_printf(out, "\t");
		}
		prt_newline(out);
	}

	if (cached) {
		prt_printf(out, "cached:\t");
		prt_units_u64(out, cached << 9);
		prt_printf(out, "\r\n");
	}

	if (reserved) {
		prt_printf(out, "reserved:\t");
		prt_units_u64(out, reserved << 9);
		prt_printf(out, "\r\n");
	}
	printbuf_tabstop_finish(out);
}

static int fs_usage_v1_to_text(struct printbuf *out,
			       struct bchfs_handle fs,
			       dev_names dev_names,
			       enum fs_usage_fields fields)
{
	unsigned accounting_types =
		BIT(BCH_DISK_ACCOUNTING_replicas)|
		BIT(BCH_DISK_ACCOUNTING_persistent_reserved);

	if (fields & FS_USAGE_compression)
		accounting_types |= BIT(BCH_DISK_ACCOUNTING_compression);

	if (fields & FS_USAGE_btree)
		accounting_types |= BIT(BCH_DISK_ACCOUNTING_btree);

	if (fields & FS_USAGE_rebalance_work) {
		if (bcachefs_kernel_version() < bcachefs_metadata_version_reconcile) {
			accounting_types |= BIT(BCH_DISK_ACCOUNTING_rebalance_work);
		} else {
			accounting_types |= BIT(BCH_DISK_ACCOUNTING_reconcile_work);
			accounting_types |= BIT(BCH_DISK_ACCOUNTING_dev_leaving);
		}
	}

	struct bch_ioctl_query_accounting *a =
		bchu_fs_accounting(fs, accounting_types);
	if (!a)
		return -1;

	accounting_swab_if_old(a);

	darray_accounting_p a_sorted = {};

	accounting_sort(&a_sorted, a);
	printbuf_tabstop_start(out);

	prt_str(out, "Filesystem:\t");
	pr_uuid(out, fs.uuid.b);
	prt_newline(out);

	prt_printf(out, "Size:\t");
	prt_units_u64(out, a->capacity << 9);
	prt_newline(out);

	prt_printf(out, "Used:\t");
	prt_units_u64(out, a->used << 9);
	prt_newline(out);

	prt_printf(out, "Online reserved:\t");
	prt_units_u64(out, a->online_reserved << 9);
	prt_newline(out);

	printbuf_tabstop_finish(out);

	replicas_summary_to_text(out, a_sorted, dev_names);

	if (fields & FS_USAGE_replicas) {
		printbuf_tabstops_reset(out);
		printbuf_tabstop_start(out);
		prt_printf(out, "\nData type\tRequired/total\tDurability\tDevices\n");
	}

	unsigned prev_type = -1;

	darray_for_each(a_sorted, i) {
		struct bkey_i_accounting *a = *i;

		struct disk_accounting_pos acc_k;
		bpos_to_disk_accounting_pos(&acc_k, a->k.p);

		bool new_type = acc_k.type != prev_type;
		prev_type = acc_k.type;

		switch (acc_k.type) {
		case BCH_DISK_ACCOUNTING_persistent_reserved:
			if (fields & FS_USAGE_replicas)
				persistent_reserved_to_text(out,
							    acc_k.persistent_reserved.nr_replicas,
							    a->v.d[0]);
			break;
		case BCH_DISK_ACCOUNTING_replicas:
			if (fields & FS_USAGE_replicas)
				replicas_usage_to_text(out, &acc_k.replicas, a->v.d[0], &dev_names);
			break;
		case BCH_DISK_ACCOUNTING_compression:
			if (new_type) {
				prt_printf(out, "\nCompression:\n");
				printbuf_tabstops_reset(out);
				printbuf_tabstop_start(out);
				prt_printf(out, "type\tcompressed\tuncompressed\taverage extent size\n");
			}

			u64 nr_extents			= a->v.d[0];
			u64 sectors_uncompressed	= a->v.d[1];
			u64 sectors_compressed		= a->v.d[2];

			bch2_prt_compression_type(out, acc_k.compression.type);
			prt_printf(out, "\t");

			prt_units_u64(out, sectors_compressed << 9);
			prt_printf(out, "\t");

			prt_units_u64(out, sectors_uncompressed << 9);
			prt_printf(out, "\t");

			prt_units_u64(out, nr_extents
					       ? div_u64(sectors_uncompressed << 9, nr_extents)
					       : 0);
			prt_newline(out);
			break;
		case BCH_DISK_ACCOUNTING_btree:
			if (new_type) {
				prt_printf(out, "\nBtree usage:\n");
				printbuf_tabstops_reset(out);
				printbuf_tabstop_start(out);
			}
			prt_printf(out, "%s:\t", bch2_btree_id_str(acc_k.btree.id));
			prt_units_u64(out, a->v.d[0] << 9);
			prt_newline(out);
			break;
		case BCH_DISK_ACCOUNTING_rebalance_work:
			if (new_type)
				prt_printf(out, "\nPending rebalance work:\n");
			prt_units_u64(out, a->v.d[0] << 9);
			prt_newline(out);
			break;
		case BCH_DISK_ACCOUNTING_reconcile_work:
			if (new_type) {
				printbuf_tabstops_reset(out);
				printbuf_tabstop_start(out);
				prt_printf(out, "\nPending reconcile:\tdata\tmetadata\n");
			}
			bch2_prt_reconcile_accounting_type(out, acc_k.reconcile_work.type);
			prt_char(out, ':');
			prt_printf(out, "\t");
			prt_units_u64(out, a->v.d[0] << 9);
			prt_printf(out, "\t");
			prt_units_u64(out, a->v.d[1] << 9);
			prt_newline(out);
			break;
		}

		/* Finish elastic tabstops when type changes */
		size_t idx = i - a_sorted.data;
		if (idx + 1 < a_sorted.nr) {
			struct bkey_i_accounting *next = a_sorted.data[idx + 1];
			struct disk_accounting_pos next_k;
			bpos_to_disk_accounting_pos(&next_k, next->k.p);

			if (next_k.type != acc_k.type) {
				if (acc_k.type == BCH_DISK_ACCOUNTING_compression ||
				    acc_k.type == BCH_DISK_ACCOUNTING_btree ||
				    acc_k.type == BCH_DISK_ACCOUNTING_reconcile_work)
					printbuf_tabstop_finish(out);
			}
		} else {
			/* Last item - finish if needed */
			if (acc_k.type == BCH_DISK_ACCOUNTING_compression ||
			    acc_k.type == BCH_DISK_ACCOUNTING_btree ||
			    acc_k.type == BCH_DISK_ACCOUNTING_reconcile_work)
				printbuf_tabstop_finish(out);
		}
	}

	if (fields & FS_USAGE_replicas)
		printbuf_tabstop_finish(out);

	darray_exit(&a_sorted);
	free(a);
	return 0;
}

static void fs_usage_v0_to_text(struct printbuf *out,
				struct bchfs_handle fs,
				dev_names dev_names,
				enum fs_usage_fields fields)
{
	struct bch_ioctl_fs_usage *u = bchu_fs_usage(fs);

	prt_str(out, "Filesystem: ");
	pr_uuid(out, fs.uuid.b);
	prt_newline(out);

	printbuf_tabstops_reset(out);
	printbuf_tabstop_start(out);

	prt_str(out, "Size:");
	prt_printf(out, "\t");
	prt_units_u64(out, u->capacity << 9);
	prt_newline(out);

	prt_str(out, "Used:");
	prt_printf(out, "\t");
	prt_units_u64(out, u->used << 9);
	prt_newline(out);

	prt_str(out, "Online reserved:");
	prt_printf(out, "\t");
	prt_units_u64(out, u->online_reserved << 9);
	prt_newline(out);

	printbuf_tabstop_finish(out);

	prt_newline(out);

	printbuf_tabstops_reset(out);
	printbuf_tabstop_start(out);

	prt_str(out, "Data type");
	prt_printf(out, "\t");

	prt_str(out, "Required/total");
	prt_printf(out, "\t");

	prt_str(out, "Durability");
	prt_printf(out, "\t");

	prt_str(out, "Devices");
	prt_newline(out);

	for (unsigned i = 0; i < BCH_REPLICAS_MAX; i++)
		persistent_reserved_to_text(out, i, u->persistent_reserved[i]);

	struct bch_replicas_usage *r;

	for_each_usage_replica(u, r)
		if (r->r.data_type < BCH_DATA_user)
			replicas_usage_to_text(out, &r->r, r->sectors, &dev_names);

	for_each_usage_replica(u, r)
		if (r->r.data_type == BCH_DATA_user &&
		    r->r.nr_required <= 1)
			replicas_usage_to_text(out, &r->r, r->sectors, &dev_names);

	for_each_usage_replica(u, r)
		if (r->r.data_type == BCH_DATA_user &&
		    r->r.nr_required > 1)
			replicas_usage_to_text(out, &r->r, r->sectors, &dev_names);

	for_each_usage_replica(u, r)
		if (r->r.data_type > BCH_DATA_user)
			replicas_usage_to_text(out, &r->r, r->sectors, &dev_names);

	printbuf_tabstop_finish(out);

	free(u);
}

static void fs_usage_to_text(struct printbuf *out, const char *path,
			     enum fs_usage_fields fields)
{
	struct bchfs_handle fs = bcache_fs_open(path);

	dev_names dev_names = bchu_fs_get_devices(fs);

	if (fs_usage_v1_to_text(out, fs, dev_names, fields))
		fs_usage_v0_to_text(out, fs, dev_names, fields);

	devs_usage_to_text(out, fs, dev_names, fields & FS_USAGE_devices);

	darray_for_each(dev_names, dev) {
		free(dev->dev);
		free(dev->label);
	}
	darray_exit(&dev_names);

	bcache_fs_close(fs);
}

static void fs_usage_usage(void)
{
	puts("bcachefs fs usage - display detailed filesystem usage\n"
	     "Usage: bcachefs fs usage [OPTION]... <mountpoint>\n"
	     "\n"
	     "Options:\n"
	     "  -f, --fields=FIELDS          List of accounting sections to print:\n"
	     "                                 replicas, btree, compression, rebalance_work, devices\n"
	     "  -a                           Print all accounting fields\n"
	     "  -h, --human-readable         Human readable units\n"
	     "  -H, --help                   Display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
}

int cmd_fs_usage(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "fields",		required_argument,	NULL, 'f' },
		{ "all",		no_argument,		NULL, 'a' },
		{ "human-readable",     no_argument,            NULL, 'h' },
		{ "help",		no_argument,		NULL, 'H' },
		{ NULL }
	};
	bool human_readable = false;
	unsigned fields = 0;
	struct printbuf buf = PRINTBUF;
	char *fs;
	int opt;

	while ((opt = getopt_long(argc, argv, "f:ahH",
				  longopts, NULL)) != -1)
		switch (opt) {
		case 'f':
			fields |= read_flag_list_or_die(optarg, fs_usage_field_strs, "fields");
			break;
		case 'a':
			fields = ~0;
		case 'h':
			human_readable = true;
			break;
		case 'H':
			fs_usage_usage();
			exit(EXIT_SUCCESS);
		default:
			fs_usage_usage();
			exit(EXIT_FAILURE);
		}
	args_shift(optind);

	if (!fields)
		fields |= FS_USAGE_rebalance_work;

	if (!argc) {
		printbuf_reset(&buf);
		buf.human_readable_units = human_readable;
		fs_usage_to_text(&buf, ".", fields);
		printf("%s", buf.buf);
	} else {
		while ((fs = arg_pop())) {
			printbuf_reset(&buf);
			buf.human_readable_units = human_readable;
			fs_usage_to_text(&buf, fs, fields);
			printf("%s", buf.buf);
		}
	}

	printbuf_exit(&buf);
	return 0;
}

int fs_usage(void)
{
	puts("bcachefs fs - manage a running filesystem\n"
	     "Usage: bcachefs fs <CMD> [OPTIONS]\n"
	     "\n"
	     "Commands:\n"
	     "  usage                        Display detailed filesystem usage\n"
	     "  top                          Show runtime performance information\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	return 0;
}

int fs_cmds(int argc, char *argv[])
{
	char *cmd = pop_cmd(&argc, argv);

	if (argc < 1)
		return fs_usage();
	if (!strcmp(cmd, "usage"))
		return cmd_fs_usage(argc, argv);
	if (!strcmp(cmd, "top"))
		return cmd_fs_top(argc, argv);

	fs_usage();
	return -EINVAL;
}
