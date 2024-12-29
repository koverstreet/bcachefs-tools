#include <getopt.h>
#include <stdio.h>
#include <sys/ioctl.h>

#include <uuid/uuid.h>

#include "linux/sort.h"
#include "linux/rcupdate.h"

#include "libbcachefs/bcachefs_ioctl.h"
#include "libbcachefs/buckets.h"
#include "libbcachefs/disk_accounting.h"
#include "libbcachefs/opts.h"
#include "libbcachefs/super-io.h"

#include "cmds.h"
#include "libbcachefs.h"

#include "libbcachefs/darray.h"

static void __dev_usage_type_to_text(struct printbuf *out,
				     enum bch_data_type type,
				     unsigned bucket_size,
				     u64 buckets, u64 sectors, u64 frag)
{
	bch2_prt_data_type(out, type);
	prt_char(out, ':');
	prt_tab(out);

	prt_units_u64(out, sectors << 9);
	prt_tab_rjust(out);

	prt_printf(out, "%llu", buckets);
	prt_tab_rjust(out);

	if (frag) {
		prt_units_u64(out, frag << 9);
		prt_tab_rjust(out);
	}
	prt_newline(out);
}

static void dev_usage_type_to_text(struct printbuf *out,
				   struct bch_ioctl_dev_usage_v2 *u,
				   enum bch_data_type type)
{
	u64 sectors = 0;
	switch (type) {
	case BCH_DATA_free:
	case BCH_DATA_need_discard:
	case BCH_DATA_need_gc_gens:
		/* sectors are 0 for these types so calculate sectors for them */
		sectors = u->d[type].buckets * u->bucket_size;
		break;
	default:
		sectors = u->d[type].sectors;
	}

	__dev_usage_type_to_text(out, type,
			u->bucket_size,
			u->d[type].buckets,
			sectors,
			u->d[type].fragmented);
}

static void dev_usage_to_text(struct printbuf *out,
			      struct bchfs_handle fs,
			      struct dev_name *d)
{
	struct bch_ioctl_dev_usage_v2 *u = bchu_dev_usage(fs, d->idx);

	prt_newline(out);
	prt_printf(out, "%s (device %u):", d->label ?: "(no label)", d->idx);
	prt_tab(out);
	prt_str(out, d->dev ?: "(device not found)");
	prt_tab_rjust(out);

	prt_str(out, bch2_member_states[u->state]);
	prt_tab_rjust(out);

	prt_newline(out);

	printbuf_indent_add(out, 2);
	prt_tab(out);

	prt_str(out, "data");
	prt_tab_rjust(out);

	prt_str(out, "buckets");
	prt_tab_rjust(out);

	prt_str(out, "fragmented");
	prt_tab_rjust(out);

	prt_newline(out);

	for (unsigned i = 0; i < u->nr_data_types; i++)
		dev_usage_type_to_text(out, u, i);

	prt_str(out, "capacity:");
	prt_tab(out);

	prt_units_u64(out, (u->nr_buckets * u->bucket_size) << 9);
	prt_tab_rjust(out);
	prt_printf(out, "%llu", u->nr_buckets);
	prt_tab_rjust(out);

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

static void devs_usage_to_text(struct printbuf *out,
			       struct bchfs_handle fs,
			       dev_names dev_names)
{
	sort(dev_names.data, dev_names.nr,
	     sizeof(dev_names.data[0]), dev_by_label_cmp, NULL);

	printbuf_tabstops_reset(out);
	printbuf_tabstop_push(out, 16);
	printbuf_tabstop_push(out, 20);
	printbuf_tabstop_push(out, 16);
	printbuf_tabstop_push(out, 14);

	darray_for_each(dev_names, dev)
		dev_usage_to_text(out, fs, dev);

	darray_for_each(dev_names, dev) {
		free(dev->dev);
		free(dev->label);
	}
}

static void persistent_reserved_to_text(struct printbuf *out,
					unsigned nr_replicas, s64 sectors)
{
	if (!sectors)
		return;

	prt_str(out, "reserved:");
	prt_tab(out);
	prt_printf(out, "%u/%u ", 1, nr_replicas);
	prt_tab(out);
	prt_str(out, "[] ");
	prt_units_u64(out, sectors << 9);
	prt_tab_rjust(out);
	prt_newline(out);
}

static void replicas_usage_to_text(struct printbuf *out,
				   const struct bch_replicas_entry_v1 *r,
				   s64 sectors,
				   dev_names *dev_names)
{
	if (!sectors)
		return;

	char devs[4096], *d = devs;
	*d++ = '[';

	unsigned durability = 0;

	for (unsigned i = 0; i < r->nr_devs; i++) {
		unsigned dev_idx = r->devs[i];
		struct dev_name *dev = dev_idx_to_name(dev_names, dev_idx);

		durability += dev ? dev->durability : 0;

		if (i)
			*d++ = ' ';

		d += dev && dev->dev
			? sprintf(d, "%s", dev->dev)
			: sprintf(d, "%u", dev_idx);
	}
	*d++ = ']';
	*d++ = '\0';

	bch2_prt_data_type(out, r->data_type);
	prt_char(out, ':');
	prt_tab(out);

	prt_printf(out, "%u/%u ", r->nr_required, r->nr_devs);
	prt_tab(out);

	prt_printf(out, "%u ", durability);
	prt_tab(out);

	prt_printf(out, "%s ", devs);
	prt_tab(out);

	prt_units_u64(out, sectors << 9);
	prt_tab_rjust(out);
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

	bch2_bpos_swab(&lp);
	bch2_bpos_swab(&rp);
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

static int fs_usage_v1_to_text(struct printbuf *out,
			       struct bchfs_handle fs,
			       dev_names dev_names)
{
	struct bch_ioctl_query_accounting *a =
		bchu_fs_accounting(fs,
			BIT(BCH_DISK_ACCOUNTING_persistent_reserved)|
			BIT(BCH_DISK_ACCOUNTING_replicas)|
			BIT(BCH_DISK_ACCOUNTING_compression)|
			BIT(BCH_DISK_ACCOUNTING_btree)|
			BIT(BCH_DISK_ACCOUNTING_rebalance_work));
	if (!a)
		return -1;

	darray_accounting_p a_sorted = {};

	accounting_sort(&a_sorted, a);

	prt_str(out, "Filesystem: ");
	pr_uuid(out, fs.uuid.b);
	prt_newline(out);

	printbuf_tabstops_reset(out);
	printbuf_tabstop_push(out, 20);
	printbuf_tabstop_push(out, 16);

	prt_str(out, "Size:");
	prt_tab(out);
	prt_units_u64(out, a->capacity << 9);
	prt_tab_rjust(out);
	prt_newline(out);

	prt_str(out, "Used:");
	prt_tab(out);
	prt_units_u64(out, a->used << 9);
	prt_tab_rjust(out);
	prt_newline(out);

	prt_str(out, "Online reserved:");
	prt_tab(out);
	prt_units_u64(out, a->online_reserved << 9);
	prt_tab_rjust(out);
	prt_newline(out);

	prt_newline(out);

	printbuf_tabstops_reset(out);

	printbuf_tabstop_push(out, 16);
	prt_str(out, "Data type");
	prt_tab(out);

	printbuf_tabstop_push(out, 16);
	prt_str(out, "Required/total");
	prt_tab(out);

	printbuf_tabstop_push(out, 14);
	prt_str(out, "Durability");
	prt_tab(out);

	printbuf_tabstop_push(out, 14);
	prt_str(out, "Devices");
	prt_newline(out);

	printbuf_tabstop_push(out, 14);

	unsigned prev_type = 0;

	darray_for_each(a_sorted, i) {
		struct bkey_i_accounting *a = *i;

		struct disk_accounting_pos acc_k;
		bpos_to_disk_accounting_pos(&acc_k, a->k.p);

		bool new_type = acc_k.type != prev_type;
		prev_type = acc_k.type;

		switch (acc_k.type) {
		case BCH_DISK_ACCOUNTING_persistent_reserved:
			persistent_reserved_to_text(out,
				acc_k.persistent_reserved.nr_replicas,
				a->v.d[0]);
			break;
		case BCH_DISK_ACCOUNTING_replicas:
			replicas_usage_to_text(out, &acc_k.replicas, a->v.d[0], &dev_names);
			break;
		case BCH_DISK_ACCOUNTING_compression:
			if (new_type) {
				prt_printf(out, "\nCompression:\n");
				printbuf_tabstops_reset(out);
				printbuf_tabstop_push(out, 12);
				printbuf_tabstop_push(out, 16);
				printbuf_tabstop_push(out, 16);
				printbuf_tabstop_push(out, 24);
				prt_printf(out, "type\tcompressed\runcompressed\raverage extent size\r\n");
			}

			u64 nr_extents			= a->v.d[0];
			u64 sectors_uncompressed	= a->v.d[1];
			u64 sectors_compressed		= a->v.d[2];

			bch2_prt_compression_type(out, acc_k.compression.type);
			prt_tab(out);

			prt_human_readable_u64(out, sectors_compressed << 9);
			prt_tab_rjust(out);

			prt_human_readable_u64(out, sectors_uncompressed << 9);
			prt_tab_rjust(out);

			prt_human_readable_u64(out, nr_extents
					       ? div_u64(sectors_uncompressed << 9, nr_extents)
					       : 0);
			prt_tab_rjust(out);
			prt_newline(out);
			break;
		case BCH_DISK_ACCOUNTING_btree:
			if (new_type) {
				prt_printf(out, "\nBtree usage:\n");
				printbuf_tabstops_reset(out);
				printbuf_tabstop_push(out, 12);
				printbuf_tabstop_push(out, 16);
			}
			prt_printf(out, "%s:\t", bch2_btree_id_str(acc_k.btree.id));
			prt_units_u64(out, a->v.d[0] << 9);
			prt_tab_rjust(out);
			prt_newline(out);
			break;
		case BCH_DISK_ACCOUNTING_rebalance_work:
			if (new_type)
				prt_printf(out, "\nPending rebalance work:\n");
			prt_units_u64(out, a->v.d[0] << 9);
			prt_newline(out);
			break;
		}
	}

	darray_exit(&a_sorted);
	free(a);
	return 0;
}

static void fs_usage_v0_to_text(struct printbuf *out,
				struct bchfs_handle fs,
				dev_names dev_names)
{
	struct bch_ioctl_fs_usage *u = bchu_fs_usage(fs);

	prt_str(out, "Filesystem: ");
	pr_uuid(out, fs.uuid.b);
	prt_newline(out);

	printbuf_tabstops_reset(out);
	printbuf_tabstop_push(out, 20);
	printbuf_tabstop_push(out, 16);

	prt_str(out, "Size:");
	prt_tab(out);
	prt_units_u64(out, u->capacity << 9);
	prt_tab_rjust(out);
	prt_newline(out);

	prt_str(out, "Used:");
	prt_tab(out);
	prt_units_u64(out, u->used << 9);
	prt_tab_rjust(out);
	prt_newline(out);

	prt_str(out, "Online reserved:");
	prt_tab(out);
	prt_units_u64(out, u->online_reserved << 9);
	prt_tab_rjust(out);
	prt_newline(out);

	prt_newline(out);

	printbuf_tabstops_reset(out);

	printbuf_tabstop_push(out, 16);
	prt_str(out, "Data type");
	prt_tab(out);

	printbuf_tabstop_push(out, 16);
	prt_str(out, "Required/total");
	prt_tab(out);

	printbuf_tabstop_push(out, 14);
	prt_str(out, "Durability");
	prt_tab(out);

	printbuf_tabstop_push(out, 14);
	prt_str(out, "Devices");
	prt_newline(out);

	printbuf_tabstop_push(out, 14);

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

	free(u);
}

static void fs_usage_to_text(struct printbuf *out, const char *path)
{
	struct bchfs_handle fs = bcache_fs_open(path);

	dev_names dev_names = bchu_fs_get_devices(fs);

	if (!fs_usage_v1_to_text(out, fs, dev_names))
		goto devs;

	fs_usage_v0_to_text(out, fs, dev_names);
devs:
	devs_usage_to_text(out, fs, dev_names);

	darray_exit(&dev_names);

	bcache_fs_close(fs);
}

static void fs_usage_usage(void)
{
	puts("bcachefs fs usage - display detailed filesystem usage\n"
	     "Usage: bcachefs fs usage [OPTION]... <mountpoint>\n"
	     "\n"
	     "Options:\n"
	     "  -h, --human-readable              Human readable units\n"
	     "  -H, --help                        Display this help and exit\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
}

int cmd_fs_usage(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "help",		no_argument,		NULL, 'H' },
		{ "human-readable",     no_argument,            NULL, 'h' },
		{ NULL }
	};
	bool human_readable = false;
	struct printbuf buf = PRINTBUF;
	char *fs;
	int opt;

	while ((opt = getopt_long(argc, argv, "h",
				  longopts, NULL)) != -1)
		switch (opt) {
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

	if (!argc) {
		printbuf_reset(&buf);
		buf.human_readable_units = human_readable;
		fs_usage_to_text(&buf, ".");
		printf("%s", buf.buf);
	} else {
		while ((fs = arg_pop())) {
			printbuf_reset(&buf);
			buf.human_readable_units = human_readable;
			fs_usage_to_text(&buf, fs);
			printf("%s", buf.buf);
		}
	}

	printbuf_exit(&buf);
	return 0;
}
