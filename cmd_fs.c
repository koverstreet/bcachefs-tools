
#include <stdio.h>
#include <sys/ioctl.h>

#include <uuid/uuid.h>

#include "ccan/darray/darray.h"

#include "linux/sort.h"

#include "libbcachefs/bcachefs_ioctl.h"
#include "libbcachefs/opts.h"

#include "cmds.h"
#include "libbcachefs.h"

static void print_dev_usage_type(const char *type,
				 unsigned bucket_size,
				 u64 buckets, u64 sectors,
				 enum units units)
{
	u64 frag = max((s64) buckets * bucket_size - (s64) sectors, 0LL);

	printf_pad(20, "  %s:", type);
	printf(" %15s %15llu %15s\n",
	       pr_units(sectors, units),
	       buckets,
	       pr_units(frag, units));
}

static void print_dev_usage(struct bchfs_handle fs,
			    struct dev_name *d,
			    enum units units)
{
	struct bch_ioctl_dev_usage u = bchu_dev_usage(fs, d->idx);
	unsigned i;

	printf("\n");
	printf_pad(20, "%s (device %u):", d->label ?: "(no label)", d->idx);
	printf("%30s%16s\n", d->dev ?: "(device not found)", bch2_dev_state[u.state]);

	printf("%-20s%16s%16s%16s\n",
	       "", "data", "buckets", "fragmented");

	for (i = BCH_DATA_sb; i < BCH_DATA_NR; i++)
		print_dev_usage_type(bch2_data_types[i],
				     u.bucket_size,
				     u.buckets[i],
				     u.sectors[i],
				     units);

	print_dev_usage_type("erasure coded",
			     u.bucket_size,
			     u.ec_buckets,
			     u.ec_sectors,
			     units);

	printf_pad(20, "  available:");
	printf(" %15s %15llu\n",
	       pr_units(u.available_buckets * u.bucket_size, units),
	       u.available_buckets);

	printf_pad(20, "  capacity:");
	printf(" %15s %15llu\n",
	       pr_units(u.nr_buckets * u.bucket_size, units),
	       u.nr_buckets);
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

static struct dev_name *dev_idx_to_name(dev_names *dev_names, unsigned idx)
{
	struct dev_name *dev;

	darray_foreach(dev, *dev_names)
		if (dev->idx == idx)
			return dev;

	return NULL;
}

static void print_replicas_usage(const struct bch_replicas_usage *r,
				 dev_names *dev_names, enum units units)
{
	unsigned i;

	if (!r->sectors)
		return;

	char devs[4096], *d = devs;
	*d++ = '[';

	for (i = 0; i < r->r.nr_devs; i++) {
		unsigned dev_idx = r->r.devs[i];
		struct dev_name *dev = dev_idx_to_name(dev_names, dev_idx);

		if (i)
			*d++ = ' ';

		d += dev && dev->dev
			? sprintf(d, "%s", dev->dev)
			: sprintf(d, "%u", dev_idx);
	}
	*d++ = ']';
	*d++ = '\0';

	printf_pad(16, "%s: ", bch2_data_types[r->r.data_type]);
	printf_pad(16, "%u/%u ", r->r.nr_required, r->r.nr_devs);
	printf_pad(32, "%s ", devs);
	printf(" %s\n", pr_units(r->sectors, units));
}

#define for_each_usage_replica(_u, _r)					\
	for (_r = (_u)->replicas;					\
	     _r != (void *) (_u)->replicas + (_u)->replica_entries_bytes;\
	     _r = replicas_usage_next(_r),				\
	     BUG_ON((void *) _r > (void *) (_u)->replicas + (_u)->replica_entries_bytes))

static void print_fs_usage(const char *path, enum units units)
{
	unsigned i;
	char uuid[40];

	struct bchfs_handle fs = bcache_fs_open(path);

	struct dev_name *dev;
	dev_names dev_names = bchu_fs_get_devices(fs);

	struct bch_ioctl_fs_usage *u = bchu_fs_usage(fs);

	uuid_unparse(fs.uuid.b, uuid);
	printf("Filesystem %s:\n", uuid);

	printf("%-20s%12s\n", "Size:", pr_units(u->capacity, units));
	printf("%-20s%12s\n", "Used:", pr_units(u->used, units));

	printf("%-20s%12s\n", "Online reserved:", pr_units(u->online_reserved, units));

	printf("\n");
	printf("%-16s%-16s%s\n", "Data type", "Required/total", "Devices");

	for (i = 0; i < BCH_REPLICAS_MAX; i++) {
		if (!u->persistent_reserved[i])
			continue;

		printf_pad(16, "%s: ", "reserved");
		printf_pad(16, "%u/%u ", 1, i);
		printf_pad(32, "[] ");
		printf("%s\n", pr_units(u->persistent_reserved[i], units));
	}

	struct bch_replicas_usage *r;

	for_each_usage_replica(u, r)
		if (r->r.data_type < BCH_DATA_user)
			print_replicas_usage(r, &dev_names, units);

	for_each_usage_replica(u, r)
		if (r->r.data_type == BCH_DATA_user &&
		    r->r.nr_required <= 1)
			print_replicas_usage(r, &dev_names, units);

	for_each_usage_replica(u, r)
		if (r->r.data_type == BCH_DATA_user &&
		    r->r.nr_required > 1)
			print_replicas_usage(r, &dev_names, units);

	for_each_usage_replica(u, r)
		if (r->r.data_type > BCH_DATA_user)
			print_replicas_usage(r, &dev_names, units);

	free(u);

	sort(&darray_item(dev_names, 0), darray_size(dev_names),
	     sizeof(darray_item(dev_names, 0)), dev_by_label_cmp, NULL);

	darray_foreach(dev, dev_names)
		print_dev_usage(fs, dev, units);

	darray_foreach(dev, dev_names) {
		free(dev->dev);
		free(dev->label);
	}
	darray_free(dev_names);

	bcache_fs_close(fs);
}

int cmd_fs_usage(int argc, char *argv[])
{
	enum units units = BYTES;
	char *fs;
	int opt;

	while ((opt = getopt(argc, argv, "h")) != -1)
		switch (opt) {
		case 'h':
			units = HUMAN_READABLE;
			break;
		}
	args_shift(optind);

	if (!argc) {
		print_fs_usage(".", units);
	} else {
		while ((fs = arg_pop()))
			print_fs_usage(fs, units);
	}

	return 0;
}
