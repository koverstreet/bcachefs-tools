#include <dirent.h>
#include <sys/xattr.h>
#include <linux/dcache.h>
#include <linux/sort.h>
#include <linux/xattr.h>

#include "posix_to_bcachefs.h"
#include "libbcachefs/alloc_foreground.h"
#include "libbcachefs/buckets.h"
#include "libbcachefs/io_write.h"
#include "libbcachefs/namei.h"
#include "libbcachefs/str_hash.h"
#include "libbcachefs/xattr.h"

void update_inode(struct bch_fs *c,
			 struct bch_inode_unpacked *inode)
{
	struct bkey_inode_buf packed;
	int ret;

	bch2_inode_pack(&packed, inode);
	packed.inode.k.p.snapshot = U32_MAX;
	ret = bch2_btree_insert(c, BTREE_ID_inodes, &packed.inode.k_i,
				NULL, 0, BTREE_ITER_cached);
	if (ret)
		die("error updating inode: %s", bch2_err_str(ret));
}

void create_link(struct bch_fs *c,
			struct bch_inode_unpacked *parent,
			const char *name, u64 inum, mode_t mode)
{
	struct qstr qstr = QSTR(name);
	struct bch_inode_unpacked parent_u;
	struct bch_inode_unpacked inode;

	int ret = bch2_trans_commit_do(c, NULL, NULL, 0,
		bch2_link_trans(trans,
				(subvol_inum) { 1, parent->bi_inum }, &parent_u,
				(subvol_inum) { 1, inum }, &inode, &qstr));
	if (ret)
		die("error creating hardlink: %s", bch2_err_str(ret));
}

struct bch_inode_unpacked create_file(struct bch_fs *c,
					     struct bch_inode_unpacked *parent,
					     const char *name,
					     uid_t uid, gid_t gid,
					     mode_t mode, dev_t rdev)
{
	struct qstr qstr = QSTR(name);
	struct bch_inode_unpacked new_inode;

	bch2_inode_init_early(c, &new_inode);

	int ret = bch2_trans_commit_do(c, NULL, NULL, 0,
		bch2_create_trans(trans,
				  (subvol_inum) { 1, parent->bi_inum }, parent,
				  &new_inode, &qstr,
				  uid, gid, mode, rdev, NULL, NULL,
				  (subvol_inum) {}, 0));
	if (ret)
		die("error creating %s: %s", name, bch2_err_str(ret));

	return new_inode;
}

#define for_each_xattr_handler(handlers, handler)		\
	if (handlers)						\
		for ((handler) = *(handlers)++;			\
			(handler) != NULL;			\
			(handler) = *(handlers)++)

static const struct xattr_handler *xattr_resolve_name(char **name)
{
	const struct xattr_handler * const *handlers = bch2_xattr_handlers;
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

void copy_times(struct bch_fs *c, struct bch_inode_unpacked *dst,
		       struct stat *src)
{
	dst->bi_atime = timespec_to_bch2_time(c, src->st_atim);
	dst->bi_mtime = timespec_to_bch2_time(c, src->st_mtim);
	dst->bi_ctime = timespec_to_bch2_time(c, src->st_ctim);
}

void copy_xattrs(struct bch_fs *c, struct bch_inode_unpacked *dst,
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
		if (IS_ERR(h))
			continue;

		int ret = bch2_trans_commit_do(c, NULL, NULL, 0,
				bch2_xattr_set(trans,
					       (subvol_inum) { 1, dst->bi_inum },
					       dst, &hash_info, attr,
					       val, val_size, h->flags, 0));
		if (ret < 0)
			die("error creating xattr: %s", bch2_err_str(ret));
	}
}

#define WRITE_DATA_BUF	(1 << 20)

static char buf[WRITE_DATA_BUF] __aligned(PAGE_SIZE);

static void write_data(struct bch_fs *c,
		       struct bch_inode_unpacked *dst_inode,
		       u64 dst_offset, void *buf, size_t len)
{
	struct bch_write_op op;
	struct bio_vec bv[WRITE_DATA_BUF / PAGE_SIZE];

	BUG_ON(dst_offset	& (block_bytes(c) - 1));
	BUG_ON(len		& (block_bytes(c) - 1));
	BUG_ON(len > WRITE_DATA_BUF);

	bio_init(&op.wbio.bio, NULL, bv, ARRAY_SIZE(bv), 0);
	bch2_bio_map(&op.wbio.bio, buf, len);

	bch2_write_op_init(&op, c, bch2_opts_to_inode_opts(c->opts));
	op.write_point	= writepoint_hashed(0);
	op.nr_replicas	= 1;
	op.subvol	= 1;
	op.pos		= SPOS(dst_inode->bi_inum, dst_offset >> 9, U32_MAX);
	op.flags |= BCH_WRITE_sync;

	int ret = bch2_disk_reservation_get(c, &op.res, len >> 9,
					    c->opts.data_replicas, 0);
	if (ret)
		die("error reserving space in new filesystem: %s", bch2_err_str(ret));

	closure_call(&op.cl, bch2_write, NULL, NULL);

	BUG_ON(!(op.flags & BCH_WRITE_submitted));
	dst_inode->bi_sectors += len >> 9;

	if (op.error)
		die("write error: %s", bch2_err_str(op.error));
}

void copy_data(struct bch_fs *c,
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
		BKEY_PADDED_ONSTACK(k, BKEY_EXTENT_VAL_U64s_MAX) k;
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
		e->k.p.snapshot	= U32_MAX;
		e->k.size	= sectors;
		bch2_bkey_append_ptr(&e->k_i, (struct bch_extent_ptr) {
					.offset = physical,
					.dev = 0,
					.gen = *bucket_gen(ca, b),
				  });

		ret = bch2_disk_reservation_get(c, &res, sectors, 1,
						BCH_DISK_RESERVATION_NOFAIL);
		if (ret)
			die("error reserving space in new filesystem: %s",
			    bch2_err_str(ret));

		ret = bch2_btree_insert(c, BTREE_ID_extents, &e->k_i, &res, 0, 0);
		if (ret)
			die("btree insert error %s", bch2_err_str(ret));

		bch2_disk_reservation_put(c, &res);

		dst->bi_sectors	+= sectors;
		logical		+= sectors;
		physical	+= sectors;
		length		-= sectors;
	}
}

void copy_link(struct bch_fs *c, struct bch_inode_unpacked *dst,
		      char *src)
{
	ssize_t i;
	ssize_t ret = readlink(src, buf, sizeof(buf));
	if (ret < 0)
		die("readlink error: %m");

	for (i = ret; i < round_up(ret, block_bytes(c)); i++)
		buf[i] = 0;

	write_data(c, dst, 0, buf, round_up(ret, block_bytes(c)));
}

static void copy_file(struct bch_fs *c, struct bch_inode_unpacked *dst,
		      int src_fd, u64 src_size,
		      char *src_path, struct copy_fs_state *s,
		      u64 reserve_start)
{
	struct fiemap_iter iter;
	struct fiemap_extent e;

	fiemap_for_each(src_fd, iter, e)
		if (e.fe_flags & FIEMAP_EXTENT_UNKNOWN) {
			fsync(src_fd);
			break;
		}
	fiemap_iter_exit(&iter);

	fiemap_for_each(src_fd, iter, e) {
		u64 src_max = roundup(src_size, block_bytes(c));

		e.fe_length = min(e.fe_length, src_max - e.fe_logical);

		if ((e.fe_logical	& (block_bytes(c) - 1)) ||
		    (e.fe_length	& (block_bytes(c) - 1)))
			die("Unaligned extent in %s - can't handle", src_path);

		if (BCH_MIGRATE_copy == s->type || (e.fe_flags & (FIEMAP_EXTENT_UNKNOWN|
				  FIEMAP_EXTENT_ENCODED|
				  FIEMAP_EXTENT_NOT_ALIGNED|
				  FIEMAP_EXTENT_DATA_INLINE))) {
			copy_data(c, dst, src_fd, e.fe_logical,
				  e.fe_logical + min(src_size - e.fe_logical,
				      e.fe_length));
			continue;
		}

		/* If the data is in bcachefs's superblock region, copy it: */
		if (e.fe_physical < reserve_start) {
			copy_data(c, dst, src_fd, e.fe_logical,
				  e.fe_logical + min(src_size - e.fe_logical,
				      e.fe_length));
			continue;
		}

		if ((e.fe_physical	& (block_bytes(c) - 1)))
			die("Unaligned extent in %s - can't handle", src_path);

		range_add(&s->extents, e.fe_physical, e.fe_length);
		link_data(c, dst, e.fe_logical, e.fe_physical, e.fe_length);
	}
	fiemap_iter_exit(&iter);
}

static int dirent_cmp(const void *_l, const void *_r)
{
	const struct dirent *l = _l;
	const struct dirent *r = _r;

	return strcmp(l->d_name, r->d_name);
}

static void copy_dir(struct copy_fs_state *s,
		     struct bch_fs *c,
		     struct bch_inode_unpacked *dst,
		     int src_fd, const char *src_path,
		     u64 reserve_start)
{
	DIR *dir = fdopendir(src_fd);
	struct dirent *d;
	DARRAY(struct dirent) dirents = {};

	while ((errno = 0), (d = readdir(dir))) {
		darray_push(&dirents, *d);
	}

	if (errno)
		die("readdir error: %m");

	sort(dirents.data, dirents.nr, sizeof(dirents.data[0]), dirent_cmp, NULL);

	darray_for_each(dirents, d) {
		struct bch_inode_unpacked inode;
		int fd;

		if (fchdir(src_fd))
			die("chdir error: %m");

		struct stat stat =
			xfstatat(src_fd, d->d_name, AT_SYMLINK_NOFOLLOW);

		if (!strcmp(d->d_name, ".") ||
		    !strcmp(d->d_name, "..") ||
		    !strcmp(d->d_name, "lost+found"))
			continue;

		if (BCH_MIGRATE_migrate == s->type && stat.st_ino == s->bcachefs_inum)
			continue;

		char *child_path = mprintf("%s/%s", src_path, d->d_name);

		if (s->type == BCH_MIGRATE_migrate && stat.st_dev != s->dev)
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

		copy_xattrs(c, &inode, d->d_name);

		/* copy xattrs */

		switch (mode_to_type(stat.st_mode)) {
		case DT_DIR:
			fd = xopen(d->d_name, O_RDONLY|O_NOATIME);
			copy_dir(s, c, &inode, fd, child_path, reserve_start);
			close(fd);
			break;
		case DT_REG:
			inode.bi_size = stat.st_size;

			fd = xopen(d->d_name, O_RDONLY|O_NOATIME);
			copy_file(c, &inode, fd, stat.st_size,
				  child_path, s, reserve_start);
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

		copy_times(c, &inode, &stat);
		update_inode(c, &inode);
next:
		free(child_path);
	}

	darray_exit(&dirents);
	closedir(dir);
}

static void reserve_old_fs_space(struct bch_fs *c,
				 struct bch_inode_unpacked *root_inode,
				 ranges *extents,
				 u64 reserve_start)
{
	struct bch_dev *ca = c->devs[0];
	struct bch_inode_unpacked dst;
	struct hole_iter iter;
	struct range i;

	dst = create_file(c, root_inode, "old_migrated_filesystem",
			  0, 0, S_IFREG|0400, 0);
	dst.bi_size = bucket_to_sector(ca, ca->mi.nbuckets) << 9;

	ranges_sort_merge(extents);

	for_each_hole(iter, *extents, bucket_to_sector(ca, ca->mi.nbuckets) << 9, i) {
		if (i.end <= reserve_start)
			continue;

		u64 start = max(i.start, reserve_start);

		link_data(c, &dst, start, start, i.end - start);
	}

	update_inode(c, &dst);
}

void copy_fs(struct bch_fs *c, int src_fd, const char *src_path,
	     struct copy_fs_state *s, u64 reserve_start)
{
	syncfs(src_fd);

	struct bch_inode_unpacked root_inode;
	int ret = bch2_inode_find_by_inum(c, (subvol_inum) { 1, BCACHEFS_ROOT_INO },
					  &root_inode);
	if (ret)
		die("error looking up root directory: %s", bch2_err_str(ret));

	if (fchdir(src_fd))
		die("chdir error: %m");

	struct stat stat = xfstat(src_fd);
	copy_times(c, &root_inode, &stat);
	copy_xattrs(c, &root_inode, ".");


	/* now, copy: */
	copy_dir(s, c, &root_inode, src_fd, src_path, reserve_start);

	if (BCH_MIGRATE_migrate == s->type)
		reserve_old_fs_space(c, &root_inode, &s->extents, reserve_start);

	update_inode(c, &root_inode);

	if (BCH_MIGRATE_migrate == s->type)
		darray_exit(&s->extents);

	genradix_free(&s->hardlinks);
}
