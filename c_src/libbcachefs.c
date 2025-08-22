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

#include <linux/fs.h>
#include <linux/mm.h>

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

void bch2_sb_layout_init(struct bch_sb_layout *l,
			 unsigned block_size,
			 unsigned bucket_size,
			 unsigned sb_size,
			 u64 sb_start, u64 sb_end,
			 bool no_sb_at_end)
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

	/*
	 * Also create a backup superblock at the end of the disk:
	 *
	 * If we're not creating a superblock at the default offset, it
	 * means we're being run from the migrate tool and we could be
	 * overwriting existing data if we write to the end of the disk:
	 */
	if (sb_start == BCH_SB_SECTOR && !no_sb_at_end) {
		u64 backup_sb = sb_end - (1 << l->sb_max_size_bits);

		backup_sb = rounddown(backup_sb, bucket_size >> 9);
		l->sb_offset[l->nr_superblocks++] = cpu_to_le64(backup_sb);
	}
}

static u64 dev_bucket_size_clamp(struct bch_opts fs_opts, u64 dev_size, u64 fs_bucket_size)
{
	u64 max_size = rounddown_pow_of_two(dev_size / (BCH_MIN_NR_NBUCKETS * 4));
	if (opt_defined(fs_opts, btree_node_size))
		max_size = max(max_size, fs_opts.btree_node_size);
	if (max_size * BCH_MIN_NR_NBUCKETS > dev_size)
		die("bucket size %llu too big for device size", max_size);

	u64 dev_bucket_size = min(max_size, fs_bucket_size);

	/*
	 * Use encoded_extent_max instead of 64k?
	 */

	while (dev_bucket_size < 64 << 10 &&
	       dev_size / (dev_bucket_size * 2) >= BCH_MIN_NR_NBUCKETS)
		dev_bucket_size *= 2;

	return dev_bucket_size;
}

u64 bch2_pick_bucket_size(struct bch_opts opts, dev_opts_list devs)
{
	/* Bucket size must be >= block size: */
	u64 bucket_size = opts.block_size;

	/* Bucket size must be >= btree node size: */
	if (opt_defined(opts, btree_node_size))
		bucket_size = max_t(u64, bucket_size, opts.btree_node_size);

	u64 min_dev_size = BCH_MIN_NR_NBUCKETS * bucket_size;
	darray_for_each(devs, i)
		if (i->fs_size < min_dev_size)
			die("cannot format %s, too small (%llu bytes, min %llu)",
			    i->path, i->fs_size, min_dev_size);

	u64 total_fs_size = 0;
	darray_for_each(devs, i)
		total_fs_size += i->fs_size;

	struct sysinfo info;
	si_meminfo(&info);

	/*
	 * Large fudge factor to allow for other fsck processes and devices
	 * being added after creation
	 */
	u64 mem_available_for_fsck = info.totalram / 8;
	u64 buckets_can_fsck = mem_available_for_fsck / (sizeof(struct bucket) * 1.5);
	u64 mem_lower_bound = roundup_pow_of_two(total_fs_size / buckets_can_fsck);

	/*
	 * Lower bound to avoid fragmenting encoded (checksummed, compressed)
	 * extents too much as they're moved:
	 */
	bucket_size = max(bucket_size, opt_get(opts, encoded_extent_max) * 4);

	/* Lower bound to ensure we can fsck: */
	bucket_size = max(bucket_size, mem_lower_bound);

	u64 perf_lower_bound = min(2ULL << 20, total_fs_size / (1ULL << 20));

	/* We also prefer larger buckets for performance, up to 2MB at 2T */
	bucket_size = max(bucket_size, perf_lower_bound);

	bucket_size = roundup_pow_of_two(bucket_size);

	return bucket_size;
}

void bch2_check_bucket_size(struct bch_opts opts, struct dev_opts *dev)
{
	if (dev->opts.bucket_size < opts.block_size)
		die("Bucket size (%u) cannot be smaller than block size (%u)",
		    dev->opts.bucket_size, opts.block_size);

	if (opt_defined(opts, btree_node_size) &&
	    dev->opts.bucket_size < opts.btree_node_size)
		die("Bucket size (%u) cannot be smaller than btree node size (%u)",
		    dev->opts.bucket_size, opts.btree_node_size);

	if (dev->nbuckets < BCH_MIN_NR_NBUCKETS)
		die("Not enough buckets: %llu, need %u (bucket size %u)",
		    dev->nbuckets, BCH_MIN_NR_NBUCKETS, dev->opts.bucket_size);
}

static unsigned parse_target(struct bch_sb_handle *sb,
			     dev_opts_list devs,
			     const char *s)
{
	if (!s)
		return 0;

	darray_for_each(devs, i)
		if (!strcmp(s, i->path))
			return dev_to_target(i - devs.data);

	int idx = bch2_disk_path_find(sb, s);
	if (idx >= 0)
		return group_to_target(idx);

	die("Invalid target %s", s);
	return 0;
}

static void bch2_opt_set_sb_all(struct bch_sb *sb, int dev_idx, struct bch_opts *opts)
{
	for (unsigned id = 0; id < bch2_opts_nr; id++) {
		u64 v = bch2_opt_defined_by_id(opts, id)
			? bch2_opt_get_by_id(opts, id)
			: bch2_opt_get_by_id(&bch2_opts_default, id);

		__bch2_opt_set_sb(sb, dev_idx, &bch2_opt_table[id], v);
	}
}

struct bch_sb *bch2_format(struct bch_opt_strs	fs_opt_strs,
			   struct bch_opts	fs_opts,
			   struct format_opts	opts,
			   dev_opts_list devs)
{
	struct bch_sb_handle sb = { NULL };

	/* calculate block size: */
	if (!opt_defined(fs_opts, block_size)) {
		unsigned max_dev_block_size = 0;
	
		darray_for_each(devs, i)
			max_dev_block_size = max(max_dev_block_size, get_blocksize(i->bdev->bd_fd));

		opt_set(fs_opts, block_size, max_dev_block_size);
	}

	if (fs_opts.block_size < 512)
		die("blocksize too small: %u, must be greater than one sector (512 bytes)",
		    fs_opts.block_size);

	/* get device size, if it wasn't specified: */
	darray_for_each(devs, i)
		if (!i->fs_size)
			i->fs_size = get_size(i->bdev->bd_fd);

	/* calculate bucket sizes: */
	u64 fs_bucket_size = bch2_pick_bucket_size(fs_opts, devs);

	darray_for_each(devs, i)
		if (!opt_defined(i->opts, bucket_size))
			opt_set(i->opts, bucket_size,
				dev_bucket_size_clamp(fs_opts, i->fs_size, fs_bucket_size));

	darray_for_each(devs, i) {
		i->nbuckets = i->fs_size / i->opts.bucket_size;
		bch2_check_bucket_size(fs_opts, i);
	}

	/* calculate btree node size: */
	if (!opt_defined(fs_opts, btree_node_size)) {
		unsigned s = bch2_opts_default.btree_node_size;

		darray_for_each(devs, i)
			s = min(s, i->opts.bucket_size);
		opt_set(fs_opts, btree_node_size, s);
	}

	if (uuid_is_null(opts.uuid.b))
		uuid_generate(opts.uuid.b);

	if (bch2_sb_realloc(&sb, 0))
		die("insufficient memory");

	sb.sb->version		= le16_to_cpu(opts.version);
	sb.sb->version_min	= le16_to_cpu(opts.version);
	sb.sb->magic		= BCHFS_MAGIC;
	sb.sb->user_uuid	= opts.uuid;
	sb.sb->nr_devices	= devs.nr;
	SET_BCH_SB_VERSION_INCOMPAT_ALLOWED(sb.sb, opts.version);

	if (opts.version > bcachefs_metadata_version_disk_accounting_big_endian)
		sb.sb->features[0] |= cpu_to_le64(BCH_SB_FEATURES_ALL);

	uuid_generate(sb.sb->uuid.b);

	if (opts.label)
		memcpy(sb.sb->label,
		       opts.label,
		       min(strlen(opts.label), sizeof(sb.sb->label)));

	bch2_opt_set_sb_all(sb.sb, -1, &fs_opts);

	struct timespec now;
	if (clock_gettime(CLOCK_REALTIME, &now))
		die("error getting current time: %m");

	sb.sb->time_base_lo	= cpu_to_le64(now.tv_sec * NSEC_PER_SEC + now.tv_nsec);
	sb.sb->time_precision	= cpu_to_le32(1);

	/* Member info: */
	struct bch_sb_field_members_v2 *mi =
		bch2_sb_field_resize(&sb, members_v2,
			(sizeof(*mi) + sizeof(struct bch_member) * devs.nr) / sizeof(u64));

	mi->member_bytes = cpu_to_le16(sizeof(struct bch_member));
	darray_for_each(devs, i) {
		unsigned idx = i - devs.data;
		struct bch_member *m = bch2_members_v2_get_mut(sb.sb, idx);

		uuid_generate(m->uuid.b);
		m->nbuckets	= cpu_to_le64(i->nbuckets);
		m->first_bucket	= 0;
		bch2_opt_set_sb_all(sb.sb, idx, &i->opts);
	}

	/* Disk labels*/
	darray_for_each(devs, i) {
		if (!i->label)
			continue;

		int idx = bch2_disk_path_find_or_create(&sb, i->label);
		if (idx < 0)
			die("error creating disk path: %s", strerror(-idx));

		/*
		 * Recompute mi and m after each sb modification: its location
		 * in memory may have changed due to reallocation.
		 */
		struct bch_member *m = bch2_members_v2_get_mut(sb.sb, (i - devs.data));
		SET_BCH_MEMBER_GROUP(m,	idx + 1);
	}

	SET_BCH_SB_FOREGROUND_TARGET(sb.sb,
		parse_target(&sb, devs, fs_opt_strs.foreground_target));
	SET_BCH_SB_BACKGROUND_TARGET(sb.sb,
		parse_target(&sb, devs, fs_opt_strs.background_target));
	SET_BCH_SB_PROMOTE_TARGET(sb.sb,
		parse_target(&sb, devs, fs_opt_strs.promote_target));
	SET_BCH_SB_METADATA_TARGET(sb.sb,
		parse_target(&sb, devs, fs_opt_strs.metadata_target));

	/* Crypt: */
	if (opts.encrypted) {
		struct bch_sb_field_crypt *crypt =
			bch2_sb_field_resize(&sb, crypt, sizeof(*crypt) / sizeof(u64));

		bch_sb_crypt_init(sb.sb, crypt, opts.passphrase);
		SET_BCH_SB_ENCRYPTION_TYPE(sb.sb, 1);
	}

	bch2_sb_members_cpy_v2_v1(&sb);

	darray_for_each(devs, i) {
		u64 size_sectors = i->fs_size >> 9;

		sb.sb->dev_idx = i - devs.data;

		if (!i->sb_offset) {
			i->sb_offset	= BCH_SB_SECTOR;
			i->sb_end	= size_sectors;
		}

		bch2_sb_layout_init(&sb.sb->layout,
				    fs_opts.block_size,
				    i->opts.bucket_size,
				    opts.superblock_size,
				    i->sb_offset, i->sb_end,
				    opts.no_sb_at_end);

		if (i->sb_offset == BCH_SB_SECTOR) {
			/* Zero start of disk */
			static const char zeroes[BCH_SB_SECTOR << 9];

			xpwrite(i->bdev->bd_fd, zeroes, BCH_SB_SECTOR << 9, 0,
				"zeroing start of disk");
		}

		bch2_super_write(i->bdev->bd_fd, sb.sb);
		xclose(i->bdev->bd_fd);
	}

	return sb.sb;
}

int bch2_format_for_device_add(struct dev_opts *dev,
			       unsigned block_size, unsigned btree_node_size)
{
	struct bch_opt_strs fs_opt_strs;
	memset(&fs_opt_strs, 0, sizeof(fs_opt_strs));

	struct bch_opts fs_opts = bch2_parse_opts(fs_opt_strs);
	opt_set(fs_opts, block_size,		block_size);
	opt_set(fs_opts, btree_node_size,	btree_node_size);

	dev_opts_list devs = {};
	darray_push(&devs, *dev);

	struct format_opts format_opts	= format_opts_default();
	struct bch_sb *sb = bch2_format(fs_opt_strs, fs_opts, format_opts, devs);
	darray_exit(&devs);
	free(sb);

	return 0;
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

			unsigned buflen = max(bs, 4096);

			char *buf = aligned_alloc(buflen, buflen);
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
	xclose(fs.ioctl_fd);
	xclose(fs.sysfs_fd);
}

static int bcache_fs_open_by_name(const char *name, struct bchfs_handle *fs)
{
	if (uuid_parse(name, fs->uuid.b))
		memset(&fs->uuid, 0, sizeof(fs->uuid));

	char *sysfs = mprintf(SYSFS_BASE "%s", name);
	fs->sysfs_fd = open(sysfs, O_RDONLY);
	free(sysfs);

	if (fs->sysfs_fd < 0)
		return -errno;

	char *minor = read_file_str(fs->sysfs_fd, "minor");
	char *ctl = mprintf("/dev/bcachefs%s-ctl", minor);
	fs->ioctl_fd = open(ctl, O_RDWR);
	free(minor);
	free(ctl);

	return fs->ioctl_fd < 0 ? -errno : 0;
}

#ifndef FS_IOC_GETFSSYSFSPATH
struct fs_sysfs_path {
	__u8			len;
	__u8			name[128];
};
#define FS_IOC_GETFSSYSFSPATH	_IOR(0x15, 1, struct fs_sysfs_path)
#endif

int bcache_fs_open_fallible(const char *path, struct bchfs_handle *fs)
{
	memset(fs, 0, sizeof(*fs));
	fs->dev_idx = -1;

	if (!uuid_parse(path, fs->uuid.b))
		return bcache_fs_open_by_name(path, fs);

	/* It's a path: */
	int path_fd = open(path, O_RDONLY);
	if (path_fd < 0)
		return -errno;

	struct bch_ioctl_query_uuid uuid;
	if (!ioctl(path_fd, BCH_IOCTL_QUERY_UUID, &uuid)) {
		/* It's a path to the mounted filesystem: */
		fs->ioctl_fd = path_fd;

		fs->uuid = uuid.uuid;

		struct fs_sysfs_path fs_sysfs_path;
		if (!ioctl(path_fd, FS_IOC_GETFSSYSFSPATH, &fs_sysfs_path)) {
			char *sysfs = mprintf("/sys/fs/%s", fs_sysfs_path.name);
			fs->sysfs_fd = xopen(sysfs, O_RDONLY);
			free(sysfs);
		} else {
			char uuid_str[40];
			uuid_unparse(uuid.uuid.b, uuid_str);

			char *sysfs = mprintf(SYSFS_BASE "%s", uuid_str);
			fs->sysfs_fd = xopen(sysfs, O_RDONLY);
			free(sysfs);
		}
		return 0;
	}

	struct bch_opts opts = bch2_opts_empty();
	char buf[1024], *uuid_str;

	struct stat stat = xstat(path);
	xclose(path_fd);

	if (S_ISBLK(stat.st_mode)) {
		char *sysfs = mprintf("/sys/dev/block/%u:%u/bcachefs",
				      major(stat.st_rdev),
				      minor(stat.st_rdev));

		ssize_t len = readlink(sysfs, buf, sizeof(buf));
		free(sysfs);

		if (len <= 0)
			goto read_super;

		char *p = strrchr(buf, '/');
		if (!p || sscanf(p + 1, "dev-%u", &fs->dev_idx) != 1)
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

		fs->dev_idx = sb.sb->dev_idx;
		uuid_str = buf;
		uuid_unparse(sb.sb->user_uuid.b, uuid_str);

		bch2_free_super(&sb);
	}

	return bcache_fs_open_by_name(uuid_str, fs);
}

struct bchfs_handle bcache_fs_open(const char *path)
{
	struct bchfs_handle fs;
	int ret = bcache_fs_open_fallible(path, &fs);
	if (ret)
		die("Error opening filesystem at %s: %s", path, strerror(-ret));
	return fs;
}

/*
 * Given a path to a block device, open the filesystem it belongs to; also
 * return the device's idx:
 */
struct bchfs_handle bchu_fs_open_by_dev(const char *path, int *idx)
{
	struct bch_opts opts = bch2_opts_empty();
	char buf[1024], *fs_str;

	struct stat stat = xstat(path);

	if (S_ISBLK(stat.st_mode)) {
		char *sysfs = mprintf("/sys/dev/block/%u:%u/bcachefs",
				      major(stat.st_rdev),
				      minor(stat.st_rdev));

		ssize_t len = readlink(sysfs, buf, sizeof(buf));
		free(sysfs);

		if (len <= 0)
			goto read_super;

		buf[len] = '\0';

		fs_str = strstr(buf, "bcachefs/");
		if (!fs_str)
			die("error parsing sysfs");

		fs_str += 9;
		char *dev_str = strchr(fs_str, '/');
		if (!dev_str)
			die("error parsing sysfs");

		*dev_str = '\0';
		dev_str++;
		if (sscanf(dev_str, "dev-%u", idx) != 1)
			die("error parsing sysfs");
	} else {
read_super:
		opt_set(opts, noexcl,	true);
		opt_set(opts, nochanges, true);

		struct bch_sb_handle sb;
		int ret = bch2_read_super(path, &opts, &sb);
		if (ret)
			die("Error opening %s: %s", path, strerror(-ret));

		*idx = sb.sb->dev_idx;
		fs_str = buf;
		uuid_unparse(sb.sb->user_uuid.b, fs_str);

		bch2_free_super(&sb);
	}

	struct bchfs_handle fs;
	int ret = bcache_fs_open_by_name(fs_str, &fs);
	if (ret)
		die("Error opening filesystem at %s (%s): %s",
		    path, fs_str, strerror(-ret));
	return fs;
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

		if (e.ret || e.p.data_type == U8_MAX)
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

	xclose(progress_fd);
	return 0;
}

/* option parsing */

#include <getopt.h>

void bch2_opt_strs_free(struct bch_opt_strs *opts)
{
	unsigned i;

	for (i = 0; i < bch2_opts_nr; i++) {
		free(opts->by_id[i]);
		opts->by_id[i] = NULL;
	}
}

static bool opt_type_filter(const struct bch_option *opt, unsigned opt_types)
{
	if (!(opt->flags & opt_types))
		return false;

	if ((opt_types & OPT_FORMAT) &&
	    !opt->set_sb && !opt->set_member)
		return false;

	return true;
}

const struct bch_option *bch2_cmdline_opt_parse(int argc, char *argv[],
						unsigned opt_types)
{
	if (optind >= argc)
		return NULL;

	if (argv[optind][0] != '-' ||
	    argv[optind][1] != '-')
		return NULL;

	char *optstr = strdup(argv[optind] + 2);
	optarg = argv[optind + 1];

	char *eq = strchr(optstr, '=');
	if (eq) {
		*eq = '\0';
		optarg = eq + 1;
	}

	if (!optarg)
		optarg = "1";


	int optid = bch2_opt_lookup(optstr);
	if (optid < 0)
		goto noopt;

	const struct bch_option *opt = bch2_opt_table + optid;
	if (!opt_type_filter(opt, opt_types))
		goto noopt;

	optind++;

	if (opt->type != BCH_OPT_BOOL) {
		if (optarg == argv[optind])
			optind++;
	} else {
		optarg = NULL;
	}

	return opt;
noopt:
	free(optstr);
	return NULL;
}

void bch_remove_arg_from_argv(int *argc, char *argv[], int index)
{
	memmove(&argv[index], &argv[index + 1], (*argc - index) * sizeof(char*));
	(*argc)--;
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
		c = 0;		\
	} while(0)
void bch2_opts_usage(unsigned opt_types)
{
	const struct bch_option *opt;
	unsigned i, c = 0, helpcol = 30;

	for (opt = bch2_opt_table;
	     opt < bch2_opt_table + bch2_opts_nr;
	     opt++) {
		if (!opt_type_filter(opt, opt_types))
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
	DIR *dir = fdopendir(dup(fs.sysfs_fd));
	struct dirent *d;
	dev_names devs;

	struct bch_sb *sb = bchu_read_super(fs, -1);

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
		} else {
			n.dev = mprintf("(offline dev %u)", n.idx);
		}

		free(block_attr);

		char *label_attr = mprintf("dev-%u/label", n.idx);
		n.label = read_file_str(fs.sysfs_fd, label_attr);
		free(label_attr);

		char *durability_attr = mprintf("dev-%u/durability", n.idx);
		n.durability = read_file_u64(fs.sysfs_fd, durability_attr);
		free(durability_attr);

		struct bch_member m = bch2_sb_member_get(sb, n.idx);
		n.state = BCH_MEMBER_STATE(&m);

		darray_push(&devs, n);
	}

	free(sb);
	closedir(dir);

	return devs;
}

struct dev_name *dev_idx_to_name(dev_names *dev_names, unsigned idx)
{
	darray_for_each(*dev_names, dev)
		if (dev->idx == idx)
			return dev;
	return NULL;
}
