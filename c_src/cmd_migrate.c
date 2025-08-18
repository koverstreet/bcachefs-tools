#include <errno.h>
#include <fcntl.h>
#include <getopt.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/types.h>
#include <sys/vfs.h>
#include <unistd.h>

#include <linux/fiemap.h>
#include <linux/fs.h>
#include <linux/stat.h>

#include <uuid/uuid.h>

#include "cmds.h"
#include "crypto.h"
#include "libbcachefs.h"
#include "posix_to_bcachefs.h"

#include <linux/dcache.h>
#include <linux/generic-radix-tree.h>
#include "libbcachefs/bcachefs.h"
#include "libbcachefs/btree_update.h"
#include "libbcachefs/buckets.h"
#include "libbcachefs/dirent.h"
#include "libbcachefs/errcode.h"
#include "libbcachefs/inode.h"
#include "libbcachefs/replicas.h"
#include "libbcachefs/super.h"

static char *dev_t_to_path(dev_t dev)
{
	char link[PATH_MAX], *p;
	int ret;

	char *sysfs_dev = mprintf("/sys/dev/block/%u:%u",
				  major(dev), minor(dev));
	ret = readlink(sysfs_dev, link, sizeof(link));
	free(sysfs_dev);

	if (ret < 0 || ret >= sizeof(link))
		die("readlink error while looking up block device: %m");

	link[ret] = '\0';

	p = strrchr(link, '/');
	if (!p)
		die("error looking up device name");
	p++;

	return mprintf("/dev/%s", p);
}

static bool path_is_fs_root(const char *path)
{
	char *line = NULL, *p, *mount;
	size_t n = 0;
	FILE *f;
	bool ret = true;

	f = fopen("/proc/self/mountinfo", "r");
	if (!f)
		die("Error getting mount information");

	while (getline(&line, &n, f) != -1) {
		p = line;

		strsep(&p, " "); /* mount id */
		strsep(&p, " "); /* parent id */
		strsep(&p, " "); /* dev */
		strsep(&p, " "); /* root */
		mount = strsep(&p, " ");
		strsep(&p, " ");

		if (mount && !strcmp(path, mount))
			goto found;
	}

	ret = false;
found:
	fclose(f);
	free(line);
	return ret;
}

static void mark_unreserved_space(struct bch_fs *c, ranges extents)
{
	struct bch_dev *ca = c->devs[0];
	struct hole_iter iter;
	struct range i;

	for_each_hole(iter, extents, bucket_to_sector(ca, ca->mi.nbuckets) << 9, i) {
		u64 b;

		if (i.start == i.end)
			return;

		b = sector_to_bucket(ca, i.start >> 9);
		do {
			set_bit(b, ca->buckets_nouse);
			b++;
		} while (bucket_to_sector(ca, b) << 9 < i.end);
	}
}

static ranges reserve_new_fs_space(const char *file_path, unsigned block_size,
				   u64 size, u64 *bcachefs_inum, dev_t dev,
				   bool force)
{
	int fd = force
		? open(file_path, O_RDWR|O_CREAT, 0600)
		: open(file_path, O_RDWR|O_CREAT|O_EXCL, 0600);
	if (fd < 0)
		die("Error creating %s for bcachefs metadata: %m",
		    file_path);

	struct stat statbuf = xfstat(fd);

	if (statbuf.st_dev != dev)
		die("bcachefs file has incorrect device");

	*bcachefs_inum = statbuf.st_ino;

	if (fallocate(fd, 0, 0, size))
		die("Error reserving space (%llu bytes) for bcachefs metadata: %m", size);

	fsync(fd);

	struct fiemap_iter iter;
	struct fiemap_extent e;
	ranges extents = { 0 };

	fiemap_for_each(fd, iter, e) {
		if (e.fe_flags & (FIEMAP_EXTENT_UNKNOWN|
				  FIEMAP_EXTENT_ENCODED|
				  FIEMAP_EXTENT_NOT_ALIGNED|
				  FIEMAP_EXTENT_DATA_INLINE))
			die("Unable to continue: metadata file not fully mapped");

		if ((e.fe_physical	& (block_size - 1)) ||
		    (e.fe_length	& (block_size - 1)))
			die("Unable to continue: unaligned extents in metadata file");

		range_add(&extents, e.fe_physical, e.fe_length);
	}
	fiemap_iter_exit(&iter);
	xclose(fd);

	ranges_sort_merge(&extents);
	return extents;
}

static void find_superblock_space(ranges extents,
				  struct format_opts opts,
				  struct dev_opts *dev)
{
	darray_for_each(extents, i) {
		u64 start = round_up(max(256ULL << 10, i->start),
				     dev->opts.bucket_size << 9);
		u64 end = round_down(i->end,
				     dev->opts.bucket_size << 9);

		/* Need space for two superblocks: */
		if (start + (opts.superblock_size << 9) * 2 <= end) {
			dev->sb_offset	= start >> 9;
			dev->sb_end	= dev->sb_offset + opts.superblock_size * 2;
			return;
		}
	}

	die("Couldn't find a valid location for superblock");
}

static void migrate_usage(void)
{
	puts("bcachefs migrate - migrate an existing filesystem to bcachefs\n"
	     "Usage: bcachefs migrate [OPTION]...\n"
	     "\n"
	     "Options:\n"
	     "  -f fs                  Root of filesystem to migrate(s)\n"
	     "      --encrypted        Enable whole filesystem encryption (chacha20/poly1305)\n"
	     "      --no_passphrase    Don't encrypt master encryption key\n"
	     "  -F                     Force, even if metadata file already exists\n"
	     "  -h                     Display this help and exit\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
}

static const struct option migrate_opts[] = {
	{ "encrypted",		no_argument, NULL, 'e' },
	{ "no_passphrase",	no_argument, NULL, 'p' },
	{ NULL }
};

static int migrate_fs(const char		*fs_path,
		      struct bch_opt_strs	fs_opt_strs,
		      struct bch_opts		fs_opts,
		      struct format_opts	format_opts,
		      bool force)
{
	if (!path_is_fs_root(fs_path))
		die("%s is not a filesystem root", fs_path);

	int fs_fd = xopen(fs_path, O_RDONLY|O_NOATIME);
	struct stat stat = xfstat(fs_fd);

	if (!S_ISDIR(stat.st_mode))
		die("%s is not a directory", fs_path);

	dev_opts_list devs = {};
	darray_push(&devs, dev_opts_default());

	struct dev_opts *dev = &devs.data[0];

	dev->path = dev_t_to_path(stat.st_dev);
	dev->file = bdev_file_open_by_path(dev->path, BLK_OPEN_READ|BLK_OPEN_WRITE, dev, NULL);

	int ret = PTR_ERR_OR_ZERO(dev->file);
	if (ret < 0)
		die("Error opening device to format %s: %s", dev->path, strerror(-ret));
	dev->bdev = file_bdev(dev->file);

	opt_set(fs_opts, block_size, get_blocksize(dev->bdev->bd_fd));

	char *file_path = mprintf("%s/bcachefs", fs_path);
	printf("Creating new filesystem on %s in space reserved at %s\n",
	       dev->path, file_path);

	dev->fs_size		= get_size(dev->bdev->bd_fd);
	opt_set(dev->opts, bucket_size, bch2_pick_bucket_size(fs_opts, devs));

	dev->nbuckets		= dev->fs_size / dev->opts.bucket_size;

	bch2_check_bucket_size(fs_opts, dev);

	u64 bcachefs_inum;
	ranges extents = reserve_new_fs_space(file_path,
				fs_opts.block_size >> 9,
				get_size(dev->bdev->bd_fd) / 10,
				&bcachefs_inum, stat.st_dev, force);

	find_superblock_space(extents, format_opts, dev);

	struct bch_sb *sb = bch2_format(fs_opt_strs, fs_opts, format_opts, devs);

	u64 sb_offset = le64_to_cpu(sb->layout.sb_offset[0]);

	if (format_opts.passphrase)
		bch2_add_key(sb, "user", "user", format_opts.passphrase);

	free(sb);

	darray_const_str dev_paths = {};
	darray_push(&dev_paths, dev->path);

	struct bch_opts opts = bch2_opts_empty();
	opt_set(opts, sb,	sb_offset);
	opt_set(opts, nostart,	true);
	opt_set(opts, noexcl,	true);

	struct bch_fs *c = bch2_fs_open(&dev_paths, &opts);
	if (IS_ERR(c))
		die("Error opening new filesystem: %s", bch2_err_str(PTR_ERR(c)));

	ret = bch2_buckets_nouse_alloc(c);
	if (ret)
		die("Error allocating buckets_nouse: %s", bch2_err_str(ret));

	mark_unreserved_space(c, extents);

	ret = bch2_fs_start(c);
	if (ret)
		die("Error starting new filesystem: %s", bch2_err_str(ret));

	struct copy_fs_state s = {
		.bcachefs_inum	= bcachefs_inum,
		.dev		= stat.st_dev,
		.extents	= extents,
		.type		= BCH_MIGRATE_migrate,
		.reserve_start	= roundup((format_opts.superblock_size * 2 + 8) << 9,
					  bucket_bytes(c->devs[0])),
	};

	BUG_ON(!s.reserve_start);

	ret = copy_fs(c, &s, fs_fd, fs_path);

	bch2_fs_stop(c);

	if (ret)
		return ret;

	printf("Migrate complete, running fsck:\n");
	opt_set(opts, nostart,	false);
	opt_set(opts, nochanges, true);
	opt_set(opts, read_only, true);

	c = bch2_fs_open(&dev_paths, &opts);
	if (IS_ERR(c))
		die("Error opening new filesystem: %s", bch2_err_str(PTR_ERR(c)));

	bch2_fs_stop(c);
	printf("fsck complete\n");

	printf("To mount the new filesystem, run\n"
	       "  mount -t bcachefs -o sb=%llu %s dir\n"
	       "\n"
	       "After verifying that the new filesystem is correct, to create a\n"
	       "superblock at the default offset and finish the migration run\n"
	       "  bcachefs migrate-superblock -d %s -o %llu\n"
	       "\n"
	       "The new filesystem will have a file at /old_migrated_filesystem\n"
	       "referencing all disk space that might be used by the existing\n"
	       "filesystem. That file can be deleted once the old filesystem is\n"
	       "no longer needed (and should be deleted prior to running\n"
	       "bcachefs migrate-superblock)\n",
	       sb_offset, dev->path, dev->path, sb_offset);

	darray_exit(&devs);
	return 0;
}

int cmd_migrate(int argc, char *argv[])
{
	struct format_opts format_opts = format_opts_default();
	char *fs_path = NULL;
	bool no_passphrase = false, force = false;
	int opt;

	struct bch_opt_strs fs_opt_strs =
		bch2_cmdline_opts_get(&argc, argv, OPT_FORMAT);
	struct bch_opts fs_opts = bch2_parse_opts(fs_opt_strs);

	while ((opt = getopt_long(argc, argv, "f:Fh",
				  migrate_opts, NULL)) != -1)
		switch (opt) {
		case 'f':
			fs_path = optarg;
			break;
		case 'e':
			format_opts.encrypted = true;
			break;
		case 'p':
			no_passphrase = true;
			break;
		case 'F':
			force = true;
			break;
		case 'h':
			migrate_usage();
			exit(EXIT_SUCCESS);
		}

	if (!fs_path) {
		migrate_usage();
		die("Please specify a filesystem to migrate");
	}

	if (format_opts.encrypted && !no_passphrase)
		format_opts.passphrase = read_passphrase_twice("Enter passphrase: ");

	int ret = migrate_fs(fs_path,
			     fs_opt_strs,
			     fs_opts,
			     format_opts, force);
	bch2_opt_strs_free(&fs_opt_strs);
	return ret;
}

static void migrate_superblock_usage(void)
{
	puts("bcachefs migrate-superblock - create default superblock after migrating\n"
	     "Usage: bcachefs migrate-superblock [OPTION]...\n"
	     "\n"
	     "Options:\n"
	     "  -d, --dev    device     Device to create superblock for\n"
	     "  -o, --offset offset     Offset of existing superblock\n"
	     "  -h, --help              Display this help and exit\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
}

int cmd_migrate_superblock(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "dev",		required_argument,	NULL, 'd' },
		{ "offset",		required_argument,	NULL, 'o' },
		{ "help",		no_argument,		NULL, 'h' },
		{ NULL }
	};
	darray_const_str devs = {};
	u64 sb_offset = 0;
	int opt, ret;

	while ((opt = getopt_long(argc, argv, "d:o:h", longopts, NULL)) != -1)
		switch (opt) {
			case 'd':
				darray_push(&devs, optarg);
				break;
			case 'o':
				ret = kstrtou64(optarg, 10, &sb_offset);
				if (ret)
					die("Invalid offset");
				break;
			case 'h':
				migrate_superblock_usage();
				exit(EXIT_SUCCESS);
		}

	if (!devs.nr)
		die("Please specify a device");

	if (!sb_offset)
		die("Please specify offset of existing superblock");

	int fd = xopen(devs.data[0], O_RDWR);
	struct bch_sb *sb = __bch2_super_read(fd, sb_offset);
	unsigned sb_size = 1U << sb->layout.sb_max_size_bits;

	if (sb->layout.nr_superblocks >= ARRAY_SIZE(sb->layout.sb_offset))
		die("Can't add superblock: no space left in superblock layout");

	for (unsigned i = 0; i < sb->layout.nr_superblocks; i++)
		if (le64_to_cpu(sb->layout.sb_offset[i]) == BCH_SB_SECTOR ||
		    le64_to_cpu(sb->layout.sb_offset[i]) == BCH_SB_SECTOR + sb_size)
			die("Superblock layout already has default superblocks");

	memmove(&sb->layout.sb_offset[2],
		&sb->layout.sb_offset[0],
		sb->layout.nr_superblocks * sizeof(u64));
	sb->layout.nr_superblocks += 2;
	sb->layout.sb_offset[0] = cpu_to_le64(BCH_SB_SECTOR);
	sb->layout.sb_offset[1] = cpu_to_le64(BCH_SB_SECTOR + sb_size);

	/* also write first 0-3.5k bytes with zeroes, ensure we blow away old
	 * superblock */
	static const char zeroes[BCH_SB_SECTOR << 9];
	xpwrite(fd, zeroes, BCH_SB_SECTOR << 9, 0, "zeroing start of disk");

	bch2_super_write(fd, sb);
	xclose(fd);

	/* mark new superblocks */

	struct bch_opts opts = bch2_opts_empty();
	opt_set(opts, nostart,	true);
	opt_set(opts, sb,	sb_offset);

	struct bch_fs *c = bch2_fs_open(&devs, &opts);
	ret =   PTR_ERR_OR_ZERO(c) ?:
		bch2_buckets_nouse_alloc(c);
	if (ret)
		die("error opening filesystem: %s", bch2_err_str(ret));

	struct bch_dev *ca = c->devs[0];
	for (u64 b = 0; bucket_to_sector(ca, b) < BCH_SB_SECTOR + sb_size * 2; b++)
		set_bit(b, ca->buckets_nouse);

	ret = bch2_fs_start(c);
	if (ret)
		die("Error starting filesystem: %s", bch2_err_str(ret));

	bch2_fs_stop(c);

	opts = bch2_opts_empty();
	opt_set(opts, fsck, true);
	opt_set(opts, fix_errors, true);

	/*
	 * Hack: the free space counters are coming out wrong after marking the
	 * new superblock, but it's just the device counters so it's
	 * inconsequential:
	 */

	c = bch2_fs_open(&devs, &opts);
	ret =   PTR_ERR_OR_ZERO(c);
	if (ret)
		die("error opening filesystem: %s", bch2_err_str(ret));
	bch2_fs_stop(c);
	return 0;
}
