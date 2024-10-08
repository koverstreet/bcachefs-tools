#include <ctype.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <libgen.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

#include <uuid/uuid.h>

#include "libbcachefs.h"
#include "crypto.h"
#include "libbcachefs/bcachefs_format.h"
#include "libbcachefs/btree_cache.h"
#include "libbcachefs/buckets.h"
#include "libbcachefs/checksum.h"
#include "libbcachefs/disk_groups.h"
#include "libbcachefs/journal_seq_blacklist.h"
#include "libbcachefs/opts.h"
#include "libbcachefs/replicas.h"
#include "libbcachefs/super-io.h"
#include "tools-util.h"

#define NSEC_PER_SEC	1000000000L

static void init_layout(struct bch_sb_layout *l,
			unsigned block_size,
			unsigned sb_size,
			u64 sb_start, u64 sb_end)
{
	u64 sb_pos = sb_start;
	unsigned i;

	memset(l, 0, sizeof(*l));

	l->magic		= BCHFS_MAGIC;
	l->layout_type		= 0;
	l->nr_superblocks	= 2;
	l->sb_max_size_bits	= ilog2(sb_size);

	/* Create two superblocks in the allowed range: */
	for (i = 0; i < l->nr_superblocks; i++) {
		if (sb_pos != BCH_SB_SECTOR)
			sb_pos = round_up(sb_pos, block_size >> 9);

		l->sb_offset[i] = cpu_to_le64(sb_pos);
		sb_pos += sb_size;
	}

	if (sb_pos > sb_end)
		die("insufficient space for superblocks: start %llu end %llu > %llu size %u",
		    sb_start, sb_pos, sb_end, sb_size);
}

/* minimum size filesystem we can create, given a bucket size: */
static u64 min_size(unsigned bucket_size)
{
	return BCH_MIN_NR_NBUCKETS * bucket_size;
}

u64 bch2_pick_bucket_size(struct bch_opts opts, struct dev_opts *dev)
{
	u64 bucket_size;

	if (dev->size < min_size(opts.block_size))
		die("cannot format %s, too small (%llu bytes, min %llu)",
		    dev->path, dev->size, min_size(opts.block_size));

	/* Bucket size must be >= block size: */
	bucket_size = opts.block_size;

	/* Bucket size must be >= btree node size: */
	if (opt_defined(opts, btree_node_size))
		bucket_size = max_t(unsigned, bucket_size,
					 opts.btree_node_size);

	/* Want a bucket size of at least 128k, if possible: */
	bucket_size = max(bucket_size, 128ULL << 10);

	if (dev->size >= min_size(bucket_size)) {
		unsigned scale = max(1,
			ilog2(dev->size / min_size(bucket_size)) / 4);

		scale = rounddown_pow_of_two(scale);

		/* max bucket size 1 mb */
		bucket_size = min(bucket_size * scale, 1ULL << 20);
	} else {
		do {
			bucket_size /= 2;
		} while (dev->size < min_size(bucket_size));
	}

	return bucket_size;
}

void bch2_check_bucket_size(struct bch_opts opts, struct dev_opts *dev)
{
	if (dev->bucket_size < opts.block_size)
		die("Bucket size (%llu) cannot be smaller than block size (%u)",
		    dev->bucket_size, opts.block_size);

	if (opt_defined(opts, btree_node_size) &&
	    dev->bucket_size < opts.btree_node_size)
		die("Bucket size (%llu) cannot be smaller than btree node size (%u)",
		    dev->bucket_size, opts.btree_node_size);

	if (dev->nbuckets < BCH_MIN_NR_NBUCKETS)
		die("Not enough buckets: %llu, need %u (bucket size %llu)",
		    dev->nbuckets, BCH_MIN_NR_NBUCKETS, dev->bucket_size);

	if (dev->bucket_size > (u32) U16_MAX << 9)
		die("Bucket size (%llu) too big (max %u)",
		    dev->bucket_size, (u32) U16_MAX << 9);
}

static unsigned parse_target(struct bch_sb_handle *sb,
			     struct dev_opts *devs, size_t nr_devs,
			     const char *s)
{
	struct dev_opts *i;
	int idx;

	if (!s)
		return 0;

	for (i = devs; i < devs + nr_devs; i++)
		if (!strcmp(s, i->path))
			return dev_to_target(i - devs);

	idx = bch2_disk_path_find(sb, s);
	if (idx >= 0)
		return group_to_target(idx);

	die("Invalid target %s", s);
	return 0;
}

struct bch_sb *bch2_format(struct bch_opt_strs	fs_opt_strs,
			   struct bch_opts	fs_opts,
			   struct format_opts	opts,
			   struct dev_opts	*devs,
			   size_t		nr_devs)
{
	struct bch_sb_handle sb = { NULL };
	struct dev_opts *i;
	unsigned max_dev_block_size = 0;
	unsigned opt_id;
	u64 min_bucket_size = U64_MAX;

	for (i = devs; i < devs + nr_devs; i++)
		max_dev_block_size = max(max_dev_block_size, get_blocksize(i->bdev->bd_fd));

	/* calculate block size: */
	if (!opt_defined(fs_opts, block_size)) {
		opt_set(fs_opts, block_size, max_dev_block_size);
	} else if (fs_opts.block_size < max_dev_block_size)
		die("blocksize too small: %u, must be greater than device blocksize %u",
		    fs_opts.block_size, max_dev_block_size);

	/* get device size, if it wasn't specified: */
	for (i = devs; i < devs + nr_devs; i++)
		if (!i->size)
			i->size = get_size(i->bdev->bd_fd);

	/* calculate bucket sizes: */
	for (i = devs; i < devs + nr_devs; i++)
		min_bucket_size = min(min_bucket_size,
			i->bucket_size ?: bch2_pick_bucket_size(fs_opts, i));

	for (i = devs; i < devs + nr_devs; i++)
		if (!i->bucket_size)
			i->bucket_size = min_bucket_size;

	for (i = devs; i < devs + nr_devs; i++) {
		i->nbuckets = i->size / i->bucket_size;
		bch2_check_bucket_size(fs_opts, i);
	}

	/* calculate btree node size: */
	if (!opt_defined(fs_opts, btree_node_size)) {
		/* 256k default btree node size */
		opt_set(fs_opts, btree_node_size, 256 << 10);

		for (i = devs; i < devs + nr_devs; i++)
			fs_opts.btree_node_size =
				min_t(unsigned, fs_opts.btree_node_size,
				      i->bucket_size);
	}

	if (uuid_is_null(opts.uuid.b))
		uuid_generate(opts.uuid.b);

	if (bch2_sb_realloc(&sb, 0))
		die("insufficient memory");

	sb.sb->version		= le16_to_cpu(opts.version);
	sb.sb->version_min	= le16_to_cpu(opts.version);
	sb.sb->magic		= BCHFS_MAGIC;
	sb.sb->user_uuid	= opts.uuid;
	sb.sb->nr_devices	= nr_devs;

	if (opts.version == bcachefs_metadata_version_current)
		sb.sb->features[0] |= cpu_to_le64(BCH_SB_FEATURES_ALL);

	uuid_generate(sb.sb->uuid.b);

	if (opts.label)
		snprintf(sb.sb->label, sizeof(sb.sb->label), "%s", opts.label);

	for (opt_id = 0;
	     opt_id < bch2_opts_nr;
	     opt_id++) {
		u64 v;

		v = bch2_opt_defined_by_id(&fs_opts, opt_id)
			? bch2_opt_get_by_id(&fs_opts, opt_id)
			: bch2_opt_get_by_id(&bch2_opts_default, opt_id);

		__bch2_opt_set_sb(sb.sb, -1, &bch2_opt_table[opt_id], v);
	}

	struct timespec now;
	if (clock_gettime(CLOCK_REALTIME, &now))
		die("error getting current time: %m");

	sb.sb->time_base_lo	= cpu_to_le64(now.tv_sec * NSEC_PER_SEC + now.tv_nsec);
	sb.sb->time_precision	= cpu_to_le32(1);

	/* Member info: */
	struct bch_sb_field_members_v2 *mi =
		bch2_sb_field_resize(&sb, members_v2,
			(sizeof(*mi) + sizeof(struct bch_member) *
			nr_devs) / sizeof(u64));
	mi->member_bytes = cpu_to_le16(sizeof(struct bch_member));
	for (i = devs; i < devs + nr_devs; i++) {
		struct bch_member *m = bch2_members_v2_get_mut(sb.sb, (i - devs));

		uuid_generate(m->uuid.b);
		m->nbuckets	= cpu_to_le64(i->nbuckets);
		m->first_bucket	= 0;
		m->bucket_size	= cpu_to_le16(i->bucket_size >> 9);

		SET_BCH_MEMBER_DISCARD(m,	i->discard);
		SET_BCH_MEMBER_DATA_ALLOWED(m,	i->data_allowed);
		SET_BCH_MEMBER_DURABILITY(m,	i->durability + 1);
	}

	/* Disk labels*/
	for (i = devs; i < devs + nr_devs; i++) {
		struct bch_member *m;
		int idx;

		if (!i->label)
			continue;

		idx = bch2_disk_path_find_or_create(&sb, i->label);
		if (idx < 0)
			die("error creating disk path: %s", strerror(-idx));

		/*
		 * Recompute mi and m after each sb modification: its location
		 * in memory may have changed due to reallocation.
		 */
		m = bch2_members_v2_get_mut(sb.sb, (i - devs));
		SET_BCH_MEMBER_GROUP(m,	idx + 1);
	}

	SET_BCH_SB_FOREGROUND_TARGET(sb.sb,
		parse_target(&sb, devs, nr_devs, fs_opt_strs.foreground_target));
	SET_BCH_SB_BACKGROUND_TARGET(sb.sb,
		parse_target(&sb, devs, nr_devs, fs_opt_strs.background_target));
	SET_BCH_SB_PROMOTE_TARGET(sb.sb,
		parse_target(&sb, devs, nr_devs, fs_opt_strs.promote_target));
	SET_BCH_SB_METADATA_TARGET(sb.sb,
		parse_target(&sb, devs, nr_devs, fs_opt_strs.metadata_target));

	/* Crypt: */
	if (opts.encrypted) {
		struct bch_sb_field_crypt *crypt =
			bch2_sb_field_resize(&sb, crypt, sizeof(*crypt) / sizeof(u64));

		bch_sb_crypt_init(sb.sb, crypt, opts.passphrase);
		SET_BCH_SB_ENCRYPTION_TYPE(sb.sb, 1);
	}

	bch2_sb_members_cpy_v2_v1(&sb);

	for (i = devs; i < devs + nr_devs; i++) {
		u64 size_sectors = i->size >> 9;

		sb.sb->dev_idx = i - devs;

		if (!i->sb_offset) {
			i->sb_offset	= BCH_SB_SECTOR;
			i->sb_end	= size_sectors;
		}

		init_layout(&sb.sb->layout, fs_opts.block_size,
			    opts.superblock_size,
			    i->sb_offset, i->sb_end);

		/*
		 * Also create a backup superblock at the end of the disk:
		 *
		 * If we're not creating a superblock at the default offset, it
		 * means we're being run from the migrate tool and we could be
		 * overwriting existing data if we write to the end of the disk:
		 */
		if (i->sb_offset == BCH_SB_SECTOR) {
			struct bch_sb_layout *l = &sb.sb->layout;
			u64 backup_sb = size_sectors - (1 << l->sb_max_size_bits);

			backup_sb = rounddown(backup_sb, i->bucket_size >> 9);
			l->sb_offset[l->nr_superblocks++] = cpu_to_le64(backup_sb);
		}

		if (i->sb_offset == BCH_SB_SECTOR) {
			/* Zero start of disk */
			static const char zeroes[BCH_SB_SECTOR << 9];

			xpwrite(i->bdev->bd_fd, zeroes, BCH_SB_SECTOR << 9, 0,
				"zeroing start of disk");
		}

		bch2_super_write(i->bdev->bd_fd, sb.sb);
		close(i->bdev->bd_fd);
	}

	return sb.sb;
}

void bch2_super_write(int fd, struct bch_sb *sb)
{
	struct nonce nonce = { 0 };
	unsigned bs = get_blocksize(fd);

	unsigned i;
	for (i = 0; i < sb->layout.nr_superblocks; i++) {
		sb->offset = sb->layout.sb_offset[i];

		if (sb->offset == BCH_SB_SECTOR) {
			/* Write backup layout */

			BUG_ON(bs > 4096);

			char *buf = aligned_alloc(bs, bs);
			xpread(fd, buf, bs, 4096 - bs);
			memcpy(buf + bs - sizeof(sb->layout),
			       &sb->layout,
			       sizeof(sb->layout));
			xpwrite(fd, buf, bs, 4096 - bs,
				"backup layout");
			free(buf);

		}

		sb->csum = csum_vstruct(NULL, BCH_SB_CSUM_TYPE(sb), nonce, sb);
		xpwrite(fd, sb, round_up(vstruct_bytes(sb), bs),
			le64_to_cpu(sb->offset) << 9,
			"superblock");
	}

	fsync(fd);
}

struct bch_sb *__bch2_super_read(int fd, u64 sector)
{
	struct bch_sb sb, *ret;

	xpread(fd, &sb, sizeof(sb), sector << 9);

	if (memcmp(&sb.magic, &BCACHE_MAGIC, sizeof(sb.magic)) &&
	    memcmp(&sb.magic, &BCHFS_MAGIC, sizeof(sb.magic)))
		die("not a bcachefs superblock");

	size_t bytes = vstruct_bytes(&sb);

	ret = malloc(bytes);

	xpread(fd, ret, bytes, sector << 9);

	return ret;
}

/* ioctl interface: */

/* Global control device: */
int bcachectl_open(void)
{
	return xopen("/dev/bcachefs-ctl", O_RDWR);
}

/* Filesystem handles (ioctl, sysfs dir): */

#define SYSFS_BASE "/sys/fs/bcachefs/"

void bcache_fs_close(struct bchfs_handle fs)
{
	close(fs.ioctl_fd);
	close(fs.sysfs_fd);
}

struct bchfs_handle bcache_fs_open(const char *path)
{
	struct bchfs_handle ret;

	if (!uuid_parse(path, ret.uuid.b)) {
		/* It's a UUID, look it up in sysfs: */
		char *sysfs = mprintf(SYSFS_BASE "%s", path);
		ret.sysfs_fd = xopen(sysfs, O_RDONLY);

		char *minor = read_file_str(ret.sysfs_fd, "minor");
		char *ctl = mprintf("/dev/bcachefs%s-ctl", minor);
		ret.ioctl_fd = xopen(ctl, O_RDWR);

		free(sysfs);
		free(minor);
		free(ctl);
	} else {
		/* It's a path: */
		ret.ioctl_fd = open(path, O_RDONLY);
		if (ret.ioctl_fd < 0)
			die("Error opening filesystem at %s: %m", path);

		struct bch_ioctl_query_uuid uuid;
		if (ioctl(ret.ioctl_fd, BCH_IOCTL_QUERY_UUID, &uuid) < 0)
			die("error opening %s: not a bcachefs filesystem", path);

		ret.uuid = uuid.uuid;

		char uuid_str[40];
		uuid_unparse(uuid.uuid.b, uuid_str);

		char *sysfs = mprintf(SYSFS_BASE "%s", uuid_str);
		ret.sysfs_fd = xopen(sysfs, O_RDONLY);
		free(sysfs);
	}

	return ret;
}

/*
 * Given a path to a block device, open the filesystem it belongs to; also
 * return the device's idx:
 */
struct bchfs_handle bchu_fs_open_by_dev(const char *path, int *idx)
{
	struct bch_opts opts = bch2_opts_empty();
	char buf[1024], *uuid_str;

	struct stat stat = xstat(path);

	if (S_ISBLK(stat.st_mode)) {
		char *sysfs = mprintf("/sys/dev/block/%u:%u/bcachefs",
				      major(stat.st_dev),
				      minor(stat.st_dev));

		ssize_t len = readlink(sysfs, buf, sizeof(buf));
		free(sysfs);

		if (len <= 0)
			goto read_super;

		char *p = strrchr(buf, '/');
		if (!p || sscanf(p + 1, "dev-%u", idx) != 1)
			die("error parsing sysfs");

		*p = '\0';
		p = strrchr(buf, '/');
		uuid_str = p + 1;
	} else {
read_super:
		opt_set(opts, noexcl,	true);
		opt_set(opts, nochanges, true);

		struct bch_sb_handle sb;
		int ret = bch2_read_super(path, &opts, &sb);
		if (ret)
			die("Error opening %s: %s", path, strerror(-ret));

		*idx = sb.sb->dev_idx;
		uuid_str = buf;
		uuid_unparse(sb.sb->user_uuid.b, uuid_str);

		bch2_free_super(&sb);
	}

	return bcache_fs_open(uuid_str);
}

int bchu_dev_path_to_idx(struct bchfs_handle fs, const char *dev_path)
{
	int idx;
	struct bchfs_handle fs2 = bchu_fs_open_by_dev(dev_path, &idx);

	if (memcmp(&fs.uuid, &fs2.uuid, sizeof(fs.uuid)))
		idx = -1;
	bcache_fs_close(fs2);
	return idx;
}

int bchu_data(struct bchfs_handle fs, struct bch_ioctl_data cmd)
{
	int progress_fd = xioctl(fs.ioctl_fd, BCH_IOCTL_DATA, &cmd);

	while (1) {
		struct bch_ioctl_data_event e;

		if (read(progress_fd, &e, sizeof(e)) != sizeof(e))
			die("error reading from progress fd %m");

		if (e.type)
			continue;

		if (e.p.data_type == U8_MAX)
			break;

		printf("\33[2K\r");

		printf("%llu%% complete: current position %s",
		       e.p.sectors_total
		       ? e.p.sectors_done * 100 / e.p.sectors_total
		       : 0,
		       bch2_data_type_str(e.p.data_type));

		switch (e.p.data_type) {
		case BCH_DATA_btree:
		case BCH_DATA_user:
			printf(" %s:%llu:%llu",
			       bch2_btree_id_str(e.p.btree_id),
			       e.p.pos.inode,
			       e.p.pos.offset);
		}

		fflush(stdout);
		sleep(1);
	}
	printf("\nDone\n");

	close(progress_fd);
	return 0;
}

/* option parsing */

void bch2_opt_strs_free(struct bch_opt_strs *opts)
{
	unsigned i;

	for (i = 0; i < bch2_opts_nr; i++) {
		free(opts->by_id[i]);
		opts->by_id[i] = NULL;
	}
}

struct bch_opt_strs bch2_cmdline_opts_get(int *argc, char *argv[],
					  unsigned opt_types)
{
	struct bch_opt_strs opts;
	unsigned i = 1;

	memset(&opts, 0, sizeof(opts));

	while (i < *argc) {
		char *optstr = strcmp_prefix(argv[i], "--");
		char *valstr = NULL, *p;
		int optid, nr_args = 1;

		if (!optstr) {
			i++;
			continue;
		}

		optstr = strdup(optstr);

		p = optstr;
		while (isalpha(*p) || *p == '_')
			p++;

		if (*p == '=') {
			*p = '\0';
			valstr = p + 1;
		}

		optid = bch2_opt_lookup(optstr);
		if (optid < 0 ||
		    !(bch2_opt_table[optid].flags & opt_types)) {
			i++;
			goto next;
		}

		if (!valstr &&
		    bch2_opt_table[optid].type != BCH_OPT_BOOL) {
			nr_args = 2;
			valstr = argv[i + 1];
		}

		if (!valstr)
			valstr = "1";

		opts.by_id[optid] = strdup(valstr);

		*argc -= nr_args;
		memmove(&argv[i],
			&argv[i + nr_args],
			sizeof(char *) * (*argc - i));
		argv[*argc] = NULL;
next:
		free(optstr);
	}

	return opts;
}

struct bch_opts bch2_parse_opts(struct bch_opt_strs strs)
{
	struct bch_opts opts = bch2_opts_empty();
	struct printbuf err = PRINTBUF;
	unsigned i;
	int ret;
	u64 v;

	for (i = 0; i < bch2_opts_nr; i++) {
		if (!strs.by_id[i])
			continue;

		ret = bch2_opt_parse(NULL,
				     &bch2_opt_table[i],
				     strs.by_id[i], &v, &err);
		if (ret < 0 && ret != -BCH_ERR_option_needs_open_fs)
			die("Invalid option %s", err.buf);

		bch2_opt_set_by_id(&opts, i, v);
	}

	printbuf_exit(&err);
	return opts;
}

#define newline(c)		\
	do {			\
		printf("\n");	\
		c = 0;	 	\
	} while(0)
void bch2_opts_usage(unsigned opt_types)
{
	const struct bch_option *opt;
	unsigned i, c = 0, helpcol = 30;



	for (opt = bch2_opt_table;
	     opt < bch2_opt_table + bch2_opts_nr;
	     opt++) {
		if (!(opt->flags & opt_types))
			continue;

		c += printf("      --%s", opt->attr.name);

		switch (opt->type) {
		case BCH_OPT_BOOL:
			break;
		case BCH_OPT_STR:
			c += printf("=(");
			for (i = 0; opt->choices[i]; i++) {
				if (i)
					c += printf("|");
				c += printf("%s", opt->choices[i]);
			}
			c += printf(")");
			break;
		default:
			c += printf("=%s", opt->hint);
			break;
		}

		if (opt->help) {
			const char *l = opt->help;

			if (c >= helpcol)
				newline(c);

			while (1) {
				const char *n = strchrnul(l, '\n');

				while (c < helpcol) {
					putchar(' ');
					c++;
				}
				printf("%.*s", (int) (n - l), l);
				newline(c);

				if (!*n)
					break;
				l = n + 1;
			}
		} else {
			newline(c);
		}
	}
}

dev_names bchu_fs_get_devices(struct bchfs_handle fs)
{
	DIR *dir = fdopendir(fs.sysfs_fd);
	struct dirent *d;
	dev_names devs;

	darray_init(&devs);

	while ((errno = 0), (d = readdir(dir))) {
		struct dev_name n = { 0, NULL, NULL };

		if (sscanf(d->d_name, "dev-%u", &n.idx) != 1)
			continue;

		char *block_attr = mprintf("dev-%u/block", n.idx);

		char sysfs_block_buf[4096];
		ssize_t r = readlinkat(fs.sysfs_fd, block_attr,
				       sysfs_block_buf, sizeof(sysfs_block_buf));
		if (r > 0) {
			sysfs_block_buf[r] = '\0';
			n.dev = strdup(basename(sysfs_block_buf));
		}

		free(block_attr);

		char *label_attr = mprintf("dev-%u/label", n.idx);
		n.label = read_file_str(fs.sysfs_fd, label_attr);
		free(label_attr);

		char *durability_attr = mprintf("dev-%u/durability", n.idx);
		n.durability = read_file_u64(fs.sysfs_fd, durability_attr);
		free(durability_attr);

		darray_push(&devs, n);
	}

	closedir(dir);

	return devs;
}
