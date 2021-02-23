#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <getopt.h>
#include <string.h>
#include <sys/xattr.h>
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

#include <linux/dcache.h>
#include <linux/generic-radix-tree.h>
#include <linux/xattr.h>
#include "libbcachefs/bcachefs.h"
#include "libbcachefs/alloc_background.h"
#include "libbcachefs/alloc_foreground.h"
#include "libbcachefs/btree_update.h"
#include "libbcachefs/buckets.h"
#include "libbcachefs/dirent.h"
#include "libbcachefs/fs-common.h"
#include "libbcachefs/inode.h"
#include "libbcachefs/io.h"
#include "libbcachefs/replicas.h"
#include "libbcachefs/str_hash.h"
#include "libbcachefs/super.h"
#include "libbcachefs/xattr.h"

/* XXX cut and pasted from fsck.c */
#define QSTR(n) { { { .len = strlen(n) } }, .name = n }

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

static void update_inode(struct bch_fs *c,
			 struct bch_inode_unpacked *inode)
{
	struct bkey_inode_buf packed;
	int ret;

	bch2_inode_pack(c, &packed, inode);
	ret = bch2_btree_insert(c, BTREE_ID_INODES, &packed.inode.k_i,
				NULL, NULL, 0);
	if (ret)
		die("error updating inode: %s", strerror(-ret));
}

static void create_link(struct bch_fs *c,
			struct bch_inode_unpacked *parent,
			const char *name, u64 inum, mode_t mode)
{
	struct qstr qstr = QSTR(name);
	struct bch_inode_unpacked parent_u;
	struct bch_inode_unpacked inode;

	int ret = bch2_trans_do(c, NULL, NULL, 0,
		bch2_link_trans(&trans, parent->bi_inum, inum,
				&parent_u, &inode, &qstr));
	if (ret)
		die("error creating hardlink: %s", strerror(-ret));
}

static struct bch_inode_unpacked create_file(struct bch_fs *c,
					     struct bch_inode_unpacked *parent,
					     const char *name,
					     uid_t uid, gid_t gid,
					     mode_t mode, dev_t rdev)
{
	struct qstr qstr = QSTR(name);
	struct bch_inode_unpacked new_inode;

	int ret = bch2_trans_do(c, NULL, NULL, 0,
		bch2_create_trans(&trans,
				  parent->bi_inum, parent,
				  &new_inode, &qstr,
				  uid, gid, mode, rdev, NULL, NULL));
	if (ret)
		die("error creating file: %s", strerror(-ret));

	return new_inode;
}

#define for_each_xattr_handler(handlers, handler)		\
	if (handlers)						\
		for ((handler) = *(handlers)++;			\
			(handler) != NULL;			\
			(handler) = *(handlers)++)

static const struct xattr_handler *xattr_resolve_name(char **name)
{
	const struct xattr_handler **handlers = bch2_xattr_handlers;
	const struct xattr_handler *handler;

	for_each_xattr_handler(handlers, handler) {
		char *n;

		n = strcmp_prefix(*name, xattr_prefix(handler));
		if (n) {
			if (!handler->prefix ^ !*n) {
				if (*n)
					continue;
				return ERR_PTR(-EINVAL);
			}
			*name = n;
			return handler;
		}
	}
	return ERR_PTR(-EOPNOTSUPP);
}

static void copy_times(struct bch_fs *c, struct bch_inode_unpacked *dst,
		       struct stat *src)
{
	dst->bi_atime = timespec_to_bch2_time(c, src->st_atim);
	dst->bi_mtime = timespec_to_bch2_time(c, src->st_mtim);
	dst->bi_ctime = timespec_to_bch2_time(c, src->st_ctim);
}

static void copy_xattrs(struct bch_fs *c, struct bch_inode_unpacked *dst,
			char *src)
{
	struct bch_hash_info hash_info = bch2_hash_info_init(c, dst);

	char attrs[XATTR_LIST_MAX];
	ssize_t attrs_size = llistxattr(src, attrs, sizeof(attrs));
	if (attrs_size < 0)
		die("listxattr error: %m");

	char *next, *attr;
	for (attr = attrs;
	     attr < attrs + attrs_size;
	     attr = next) {
		next = attr + strlen(attr) + 1;

		char val[XATTR_SIZE_MAX];
		ssize_t val_size = lgetxattr(src, attr, val, sizeof(val));

		if (val_size < 0)
			die("error getting xattr val: %m");

		const struct xattr_handler *h = xattr_resolve_name(&attr);

		int ret = bch2_trans_do(c, NULL, NULL, 0,
				bch2_xattr_set(&trans, dst->bi_inum, &hash_info, attr,
					       val, val_size, h->flags, 0));
		if (ret < 0)
			die("error creating xattr: %s", strerror(-ret));
	}
}

static char buf[1 << 20] __aligned(PAGE_SIZE);

static void write_data(struct bch_fs *c,
		       struct bch_inode_unpacked *dst_inode,
		       u64 dst_offset, void *buf, size_t len)
{
	struct {
		struct bch_write_op op;
		struct bio_vec bv[sizeof(buf) / PAGE_SIZE];
	} o;
	struct closure cl;

	BUG_ON(dst_offset	& (block_bytes(c) - 1));
	BUG_ON(len		& (block_bytes(c) - 1));

	closure_init_stack(&cl);

	bio_init(&o.op.wbio.bio, o.bv, ARRAY_SIZE(o.bv));
	bch2_bio_map(&o.op.wbio.bio, buf, len);

	bch2_write_op_init(&o.op, c, bch2_opts_to_inode_opts(c->opts));
	o.op.write_point	= writepoint_hashed(0);
	o.op.nr_replicas	= 1;
	o.op.pos		= POS(dst_inode->bi_inum, dst_offset >> 9);

	int ret = bch2_disk_reservation_get(c, &o.op.res, len >> 9,
					    c->opts.data_replicas, 0);
	if (ret)
		die("error reserving space in new filesystem: %s", strerror(-ret));

	closure_call(&o.op.cl, bch2_write, NULL, &cl);
	closure_sync(&cl);

	dst_inode->bi_sectors += len >> 9;
}

static void copy_data(struct bch_fs *c,
		      struct bch_inode_unpacked *dst_inode,
		      int src_fd, u64 start, u64 end)
{
	while (start < end) {
		unsigned len = min_t(u64, end - start, sizeof(buf));
		unsigned pad = round_up(len, block_bytes(c)) - len;

		xpread(src_fd, buf, len, start);
		memset(buf + len, 0, pad);

		write_data(c, dst_inode, start, buf, len + pad);
		start += len;
	}
}

static void link_data(struct bch_fs *c, struct bch_inode_unpacked *dst,
		      u64 logical, u64 physical, u64 length)
{
	struct bch_dev *ca = c->devs[0];

	BUG_ON(logical	& (block_bytes(c) - 1));
	BUG_ON(physical & (block_bytes(c) - 1));
	BUG_ON(length	& (block_bytes(c) - 1));

	logical		>>= 9;
	physical	>>= 9;
	length		>>= 9;

	BUG_ON(physical + length > bucket_to_sector(ca, ca->mi.nbuckets));

	while (length) {
		struct bkey_i_extent *e;
		__BKEY_PADDED(k, BKEY_EXTENT_VAL_U64s_MAX) k;
		u64 b = sector_to_bucket(ca, physical);
		struct disk_reservation res;
		unsigned sectors;
		int ret;

		sectors = min(ca->mi.bucket_size -
			      (physical & (ca->mi.bucket_size - 1)),
			      length);

		e = bkey_extent_init(&k.k);
		e->k.p.inode	= dst->bi_inum;
		e->k.p.offset	= logical + sectors;
		e->k.size	= sectors;
		bch2_bkey_append_ptr(&e->k_i, (struct bch_extent_ptr) {
					.offset = physical,
					.dev = 0,
					.gen = bucket(ca, b)->mark.gen,
				  });

		ret = bch2_disk_reservation_get(c, &res, sectors, 1,
						BCH_DISK_RESERVATION_NOFAIL);
		if (ret)
			die("error reserving space in new filesystem: %s",
			    strerror(-ret));

		bch2_mark_bkey_replicas(c, extent_i_to_s_c(e).s_c);

		ret = bch2_btree_insert(c, BTREE_ID_EXTENTS, &e->k_i,
					&res, NULL, 0);
		if (ret)
			die("btree insert error %s", strerror(-ret));

		bch2_disk_reservation_put(c, &res);

		dst->bi_sectors	+= sectors;
		logical		+= sectors;
		physical	+= sectors;
		length		-= sectors;
	}
}

static void copy_link(struct bch_fs *c, struct bch_inode_unpacked *dst,
		      char *src)
{
	ssize_t ret = readlink(src, buf, sizeof(buf));
	if (ret < 0)
		die("readlink error: %m");

	write_data(c, dst, 0, buf, round_up(ret, block_bytes(c)));
}

static void copy_file(struct bch_fs *c, struct bch_inode_unpacked *dst,
		      int src_fd, u64 src_size,
		      char *src_path, ranges *extents)
{
	struct fiemap_iter iter;
	struct fiemap_extent e;

	fiemap_for_each(src_fd, iter, e)
		if (e.fe_flags & FIEMAP_EXTENT_UNKNOWN) {
			fsync(src_fd);
			break;
		}

	fiemap_for_each(src_fd, iter, e) {
		if ((e.fe_logical	& (block_bytes(c) - 1)) ||
		    (e.fe_length	& (block_bytes(c) - 1)))
			die("Unaligned extent in %s - can't handle", src_path);

		if (e.fe_flags & (FIEMAP_EXTENT_UNKNOWN|
				  FIEMAP_EXTENT_ENCODED|
				  FIEMAP_EXTENT_NOT_ALIGNED|
				  FIEMAP_EXTENT_DATA_INLINE)) {
			copy_data(c, dst, src_fd, e.fe_logical,
				  min(src_size - e.fe_logical,
				      e.fe_length));
			continue;
		}

		/*
		 * if the data is below 1 MB, copy it so it doesn't conflict
		 * with bcachefs's potentially larger superblock:
		 */
		if (e.fe_physical < 1 << 20) {
			copy_data(c, dst, src_fd, e.fe_logical,
				  min(src_size - e.fe_logical,
				      e.fe_length));
			continue;
		}

		if ((e.fe_physical	& (block_bytes(c) - 1)))
			die("Unaligned extent in %s - can't handle", src_path);

		range_add(extents, e.fe_physical, e.fe_length);
		link_data(c, dst, e.fe_logical, e.fe_physical, e.fe_length);
	}
}

struct copy_fs_state {
	u64			bcachefs_inum;
	dev_t			dev;

	GENRADIX(u64)		hardlinks;
	ranges			extents;
};

static void copy_dir(struct copy_fs_state *s,
		     struct bch_fs *c,
		     struct bch_inode_unpacked *dst,
		     int src_fd, const char *src_path)
{
	DIR *dir = fdopendir(src_fd);
	struct dirent *d;

	while ((errno = 0), (d = readdir(dir))) {
		struct bch_inode_unpacked inode;
		int fd;

		if (fchdir(src_fd))
			die("chdir error: %m");

		struct stat stat =
			xfstatat(src_fd, d->d_name, AT_SYMLINK_NOFOLLOW);

		if (!strcmp(d->d_name, ".") ||
		    !strcmp(d->d_name, "..") ||
		    stat.st_ino == s->bcachefs_inum)
			continue;

		char *child_path = mprintf("%s/%s", src_path, d->d_name);

		if (stat.st_dev != s->dev)
			die("%s does not have correct st_dev!", child_path);

		u64 *dst_inum = S_ISREG(stat.st_mode)
			? genradix_ptr_alloc(&s->hardlinks, stat.st_ino, GFP_KERNEL)
			: NULL;

		if (dst_inum && *dst_inum) {
			create_link(c, dst, d->d_name, *dst_inum, S_IFREG);
			goto next;
		}

		inode = create_file(c, dst, d->d_name,
				    stat.st_uid, stat.st_gid,
				    stat.st_mode, stat.st_rdev);

		if (dst_inum)
			*dst_inum = inode.bi_inum;

		copy_times(c, &inode, &stat);
		copy_xattrs(c, &inode, d->d_name);

		/* copy xattrs */

		switch (mode_to_type(stat.st_mode)) {
		case DT_DIR:
			fd = xopen(d->d_name, O_RDONLY|O_NOATIME);
			copy_dir(s, c, &inode, fd, child_path);
			close(fd);
			break;
		case DT_REG:
			inode.bi_size = stat.st_size;

			fd = xopen(d->d_name, O_RDONLY|O_NOATIME);
			copy_file(c, &inode, fd, stat.st_size,
				  child_path, &s->extents);
			close(fd);
			break;
		case DT_LNK:
			inode.bi_size = stat.st_size;

			copy_link(c, &inode, d->d_name);
			break;
		case DT_FIFO:
		case DT_CHR:
		case DT_BLK:
		case DT_SOCK:
		case DT_WHT:
			/* nothing else to copy for these: */
			break;
		default:
			BUG();
		}

		update_inode(c, &inode);
next:
		free(child_path);
	}

	if (errno)
		die("readdir error: %m");
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
		die("Error reserving space for bcachefs metadata: %m");

	fsync(fd);

	struct fiemap_iter iter;
	struct fiemap_extent e;
	ranges extents = { NULL };

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
	close(fd);

	ranges_sort_merge(&extents);
	return extents;
}

static void reserve_old_fs_space(struct bch_fs *c,
				 struct bch_inode_unpacked *root_inode,
				 ranges *extents)
{
	struct bch_dev *ca = c->devs[0];
	struct bch_inode_unpacked dst;
	struct hole_iter iter;
	struct range i;

	dst = create_file(c, root_inode, "old_migrated_filesystem",
			  0, 0, S_IFREG|0400, 0);
	dst.bi_size = bucket_to_sector(ca, ca->mi.nbuckets) << 9;

	ranges_sort_merge(extents);

	for_each_hole(iter, *extents, bucket_to_sector(ca, ca->mi.nbuckets) << 9, i)
		link_data(c, &dst, i.start, i.start, i.end - i.start);

	update_inode(c, &dst);
}

static void copy_fs(struct bch_fs *c, int src_fd, const char *src_path,
		    u64 bcachefs_inum, ranges *extents)
{
	syncfs(src_fd);

	struct bch_inode_unpacked root_inode;
	int ret = bch2_inode_find_by_inum(c, BCACHEFS_ROOT_INO, &root_inode);
	if (ret)
		die("error looking up root directory: %s", strerror(-ret));

	if (fchdir(src_fd))
		die("chdir error: %m");

	struct stat stat = xfstat(src_fd);
	copy_times(c, &root_inode, &stat);
	copy_xattrs(c, &root_inode, ".");

	struct copy_fs_state s = {
		.bcachefs_inum	= bcachefs_inum,
		.dev		= stat.st_dev,
		.extents	= *extents,
	};

	/* now, copy: */
	copy_dir(&s, c, &root_inode, src_fd, src_path);

	reserve_old_fs_space(c, &root_inode, &s.extents);

	update_inode(c, &root_inode);

	darray_free(s.extents);
	genradix_free(&s.hardlinks);

	bch2_alloc_write(c, false);
}

static void find_superblock_space(ranges extents, struct dev_opts *dev)
{
	struct range *i;

	darray_foreach(i, extents) {
		u64 start = round_up(max(256ULL << 10, i->start),
				     dev->bucket_size << 9);
		u64 end = round_down(i->end,
				     dev->bucket_size << 9);

		if (start + (128 << 10) <= end) {
			dev->sb_offset	= start >> 9;
			dev->sb_end	= dev->sb_offset + 256;
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
	     "Report bugs to <linux-bcache@vger.kernel.org>");
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
		die("%s is not a filysestem root", fs_path);

	int fs_fd = xopen(fs_path, O_RDONLY|O_NOATIME);
	struct stat stat = xfstat(fs_fd);

	if (!S_ISDIR(stat.st_mode))
		die("%s is not a directory", fs_path);

	struct dev_opts dev = dev_opts_default();

	dev.path = dev_t_to_path(stat.st_dev);
	dev.fd = xopen(dev.path, O_RDWR);

	opt_set(fs_opts, block_size, get_blocksize(dev.path, dev.fd));

	char *file_path = mprintf("%s/bcachefs", fs_path);
	printf("Creating new filesystem on %s in space reserved at %s\n",
	       dev.path, file_path);

	bch2_pick_bucket_size(fs_opts, &dev);

	u64 bcachefs_inum;
	ranges extents = reserve_new_fs_space(file_path,
				fs_opts.block_size << 9,
				get_size(dev.path, dev.fd) / 5,
				&bcachefs_inum, stat.st_dev, force);

	find_superblock_space(extents, &dev);

	struct bch_sb *sb = bch2_format(fs_opt_strs,
					fs_opts,format_opts, &dev, 1);
	u64 sb_offset = le64_to_cpu(sb->layout.sb_offset[0]);

	if (format_opts.passphrase)
		bch2_add_key(sb, format_opts.passphrase);

	free(sb);

	struct bch_opts opts = bch2_opts_empty();
	struct bch_fs *c = NULL;
	char *path[1] = { dev.path };

	opt_set(opts, sb,	sb_offset);
	opt_set(opts, nostart,	true);
	opt_set(opts, noexcl,	true);

	c = bch2_fs_open(path, 1, opts);
	if (IS_ERR(c))
		die("Error opening new filesystem: %s", strerror(-PTR_ERR(c)));

	mark_unreserved_space(c, extents);

	int ret = bch2_fs_start(c);
	if (ret)
		die("Error starting new filesystem: %s", strerror(-ret));

	copy_fs(c, fs_fd, fs_path, bcachefs_inum, &extents);

	bch2_fs_stop(c);

	printf("Migrate complete, running fsck:\n");
	opt_set(opts, nostart,	false);
	opt_set(opts, nochanges, true);

	c = bch2_fs_open(path, 1, opts);
	if (IS_ERR(c))
		die("Error opening new filesystem: %s", strerror(-PTR_ERR(c)));

	bch2_fs_stop(c);
	printf("fsck complete\n");

	printf("To mount the new filesystem, run\n"
	       "  mount -t bcachefs -o sb=%llu %s dir\n"
	       "\n"
	       "After verifying that the new filesystem is correct, to create a\n"
	       "superblock at the default offset and finish the migration run\n"
	       "  bcachefs migrate-superblock -d %s -o %llu\n"
	       "\n"
	       "The new filesystem will have a file at /old_migrated_filestem\n"
	       "referencing all disk space that might be used by the existing\n"
	       "filesystem. That file can be deleted once the old filesystem is\n"
	       "no longer needed (and should be deleted prior to running\n"
	       "bcachefs migrate-superblock)\n",
	       sb_offset, dev.path, dev.path, sb_offset);
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

	if (!fs_path)
		die("Please specify a filesystem to migrate");

	if (format_opts.encrypted && !no_passphrase)
		format_opts.passphrase = read_passphrase_twice("Enter passphrase: ");

	return migrate_fs(fs_path,
			  fs_opt_strs,
			  fs_opts,
			  format_opts, force);
}

static void migrate_superblock_usage(void)
{
	puts("bcachefs migrate-superblock - create default superblock after migrating\n"
	     "Usage: bcachefs migrate-superblock [OPTION]...\n"
	     "\n"
	     "Options:\n"
	     "  -d device     Device to create superblock for\n"
	     "  -o offset     Offset of existing superblock\n"
	     "  -h            Display this help and exit\n"
	     "Report bugs to <linux-bcache@vger.kernel.org>");
}

int cmd_migrate_superblock(int argc, char *argv[])
{
	char *dev = NULL;
	u64 offset = 0;
	int opt, ret;

	while ((opt = getopt(argc, argv, "d:o:h")) != -1)
		switch (opt) {
			case 'd':
				dev = optarg;
				break;
			case 'o':
				ret = kstrtou64(optarg, 10, &offset);
				if (ret)
					die("Invalid offset");
				break;
			case 'h':
				migrate_superblock_usage();
				exit(EXIT_SUCCESS);
		}

	if (!dev)
		die("Please specify a device");

	if (!offset)
		die("Please specify offset of existing superblock");

	int fd = xopen(dev, O_RDWR);
	struct bch_sb *sb = __bch2_super_read(fd, offset);

	if (sb->layout.nr_superblocks >= ARRAY_SIZE(sb->layout.sb_offset))
		die("Can't add superblock: no space left in superblock layout");

	unsigned i;
	for (i = 0; i < sb->layout.nr_superblocks; i++)
		if (le64_to_cpu(sb->layout.sb_offset[i]) == BCH_SB_SECTOR)
			die("Superblock layout already has default superblock");

	memmove(&sb->layout.sb_offset[1],
		&sb->layout.sb_offset[0],
		sb->layout.nr_superblocks * sizeof(u64));
	sb->layout.nr_superblocks++;

	sb->layout.sb_offset[0] = cpu_to_le64(BCH_SB_SECTOR);

	bch2_super_write(fd, sb);
	close(fd);

	return 0;
}
