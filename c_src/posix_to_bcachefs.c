#include <dirent.h>
#include <sys/xattr.h>
#include <linux/dcache.h>
#include <linux/sort.h>
#include <linux/xattr.h>

#include "posix_to_bcachefs.h"

#include "alloc/foreground.h"
#include "alloc/buckets.h"

#include "data/io_misc.h"
#include "data/read.h"
#include "data/write.h"

#include "fs/namei.h"
#include "fs/str_hash.h"
#include "fs/xattr.h"

struct hardlink {
	struct rhash_head	hash;
	u64			src, dst;
};

static const struct rhashtable_params hardlink_params = {
	.head_offset		= offsetof(struct hardlink, hash),
	.key_offset		= offsetof(struct hardlink, src),
	.key_len		= sizeof(u64),
	.automatic_shrinking	= true,
};

static int unlink_and_rm(struct bch_fs *c,
			 subvol_inum dir_inum,
			 struct bch_inode_unpacked *dir,
			 const char *child_name)
{
	struct qstr child_name_q = QSTR_INIT(child_name, strlen(child_name));

	struct bch_inode_unpacked child;
	int ret = bch2_trans_commit_do(c, NULL, NULL,
			BCH_TRANS_COMMIT_no_enospc,
		bch2_unlink_trans(trans, dir_inum, dir, &child, &child_name_q, false));
	bch_err_msg(c, ret, "unlinking %s", child_name);
	if (ret)
		return ret;

	if (!(child.bi_flags & BCH_INODE_unlinked))
		return 0;

	subvol_inum child_inum = dir_inum;
	child_inum.inum = child.bi_inum;

	ret = bch2_inode_rm(c, child_inum);
	bch_err_msg(c, ret, "deleting %s", child_name);
	return ret;
}

static void update_inode(struct bch_fs *c,
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

static int create_or_update_link(struct bch_fs *c,
				 subvol_inum dir_inum,
				 struct bch_inode_unpacked *dir,
				 const char *name, subvol_inum inum, mode_t mode)
{
	struct bch_hash_info dir_hash;
	try(bch2_hash_info_init(c, dir, &dir_hash));

	struct qstr qstr = QSTR(name);
	struct bch_inode_unpacked dir_u;
	struct bch_inode_unpacked inode;

	subvol_inum old_inum;
	int ret = bch2_dirent_lookup(c, dir_inum, &dir_hash, &qstr, &old_inum);
	if (bch2_err_matches(ret, ENOENT))
		goto create;
	if (ret)
		return ret;

	if (subvol_inum_eq(inum, old_inum))
		return 0;

	ret = unlink_and_rm(c, dir_inum, dir, name);
	if (ret)
		return ret;
create:
	ret = bch2_trans_commit_do(c, NULL, NULL, 0,
		bch2_link_trans(trans,
				dir_inum, &dir_u,
				inum, &inode, &qstr));
	bch_err_msg(c, ret, "error creating hardlink %s", name);
	return ret;
}

static struct bch_inode_unpacked create_or_update_file(struct bch_fs *c,
			subvol_inum dir_inum,
			struct bch_inode_unpacked *dir,
			const char *name,
			uid_t uid, gid_t gid,
			mode_t mode, dev_t rdev)
{
	struct bch_hash_info dir_hash;
	int ret = bch2_hash_info_init(c, dir, &dir_hash);
	if (ret)
		die("hash_info_init() error: %s", bch2_err_str(ret));

	struct qstr qname = QSTR(name);
	struct bch_inode_unpacked child_inode;
	subvol_inum child_inum;

	ret = bch2_dirent_lookup(c, dir_inum, &dir_hash, &qname, &child_inum);
	if (!ret) {
		/* Already exists, update */

		ret = bch2_inode_find_by_inum(c, child_inum, &child_inode);
		bch_err_fn(c, ret);
		if (ret)
			die("error looking up %s: %s", name, bch2_err_str(ret));

		BUG_ON(mode_to_type(child_inode.bi_mode) !=
		       mode_to_type(mode));

		child_inode.bi_mode	= mode;
		child_inode.bi_uid	= uid;
		child_inode.bi_gid	= gid;
		child_inode.bi_dev	= rdev;

		ret = bch2_trans_run(c, bch2_fsck_write_inode(trans, &child_inode));
		if (ret)
			die("error updating up %s: %s", name, bch2_err_str(ret));
	} else {
		bch2_inode_init_early(c, &child_inode);

		int ret = bch2_trans_commit_do(c, NULL, NULL, 0,
			bch2_create_trans(trans,
					  dir_inum, dir,
					  &child_inode, &qname,
					  uid, gid, mode, rdev, NULL, NULL,
					  (subvol_inum) {}, 0));
		if (ret)
			die("error creating %s: %s", name, bch2_err_str(ret));
	}

	return child_inode;
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
					       dst, attr,
					       val, val_size, h->flags, 0));
		if (ret < 0)
			die("error creating xattr: %s", bch2_err_str(ret));
	}
}

#define WRITE_DATA_BUF	(1 << 20)

static char src_buf[WRITE_DATA_BUF] __aligned(PAGE_SIZE);
static char dst_buf[WRITE_DATA_BUF] __aligned(PAGE_SIZE);

static void read_data_endio(struct bio *bio)
{
	closure_put(bio->bi_private);
}

static void read_data(struct bch_fs *c,
		      subvol_inum inum,
		      struct bch_inode_unpacked *inode,
		      u64 offset, void *buf, size_t len)
{
	BUG_ON(offset	& (block_bytes(c) - 1));
	BUG_ON(len	& (block_bytes(c) - 1));
	BUG_ON(len > WRITE_DATA_BUF);

	struct closure cl;
	closure_init_stack(&cl);

	struct bch_read_bio rbio;
	struct bio_vec bv[WRITE_DATA_BUF / PAGE_SIZE];

	bio_init(&rbio.bio, NULL, bv, ARRAY_SIZE(bv), 0);
	rbio.bio.bi_opf			= REQ_OP_READ|REQ_SYNC;
	rbio.bio.bi_iter.bi_sector	= offset >> 9;
	rbio.bio.bi_private		= &cl;
	bch2_bio_map(&rbio.bio, buf, len);

	struct bch_inode_opts opts;
	bch2_inode_opts_get_inode(c, inode, &opts);

	rbio_init(&rbio.bio, c, opts, read_data_endio);

	closure_get(&cl);
	bch2_read(c, &rbio, inum);
	closure_sync(&cl);

	if (rbio.ret)
		die("read error: %s", bch2_err_str(rbio.ret));
}

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

	struct bch_inode_opts opts;
	bch2_inode_opts_get(c, &opts, false);

	bch2_write_op_init(&op, c, opts);
	op.write_point	= writepoint_hashed(0);
	op.nr_replicas	= 1;
	op.subvol	= 1;
	op.pos		= SPOS(dst_inode->bi_inum, dst_offset >> 9, U32_MAX);
	op.flags |= BCH_WRITE_sync|BCH_WRITE_only_specified_devs;

	int ret = bch2_disk_reservation_get(c, &op.res, len >> 9,
					    c->opts.data_replicas, 0);
	if (ret)
		die("error reserving space in new filesystem: %s", bch2_err_str(ret));

	closure_call(&op.cl, bch2_write, NULL, NULL);

	BUG_ON(!(op.flags & BCH_WRITE_submitted));
	dst_inode->bi_sectors += op.i_sectors_delta;

	if (op.error)
		die("write error: %s", bch2_err_str(op.error));
}

static void copy_data(struct bch_fs *c,
		      struct bch_inode_unpacked *dst_inode,
		      int src_fd, u64 start, u64 end)
{
	while (start < end) {
		unsigned len = min_t(u64, end - start, sizeof(src_buf));
		unsigned pad = round_up(len, block_bytes(c)) - len;

		xpread(src_fd, src_buf, len, start);
		memset(src_buf + len, 0, pad);

		write_data(c, dst_inode, start, src_buf, len + pad);
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
		bch2_bkey_append_ptr(c, &e->k_i, (struct bch_extent_ptr) {
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

static void copy_link(struct bch_fs *c,
		      subvol_inum dst_inum,
		      struct bch_inode_unpacked *dst,
		      char *src)
{
	s64 i_sectors_delta = 0;
	int ret = bch2_fpunch(c, dst_inum, 0, U64_MAX, &i_sectors_delta);
	if (ret)
		die("bch2_fpunch error: %s", bch2_err_str(ret));

	dst->bi_sectors += i_sectors_delta;

	ret = readlink(src, src_buf, sizeof(src_buf));
	if (ret < 0)
		die("readlink error: %m");

	for (unsigned i = ret; i < round_up(ret, block_bytes(c)); i++)
		src_buf[i] = 0;

	write_data(c, dst, 0, src_buf, round_up(ret, block_bytes(c)));
}

static void link_file_data(struct bch_fs *c,
			   struct copy_fs_state *s,
			   struct bch_inode_unpacked *dst,
			   int src_fd, char *src_path, u64 src_size)
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
		s->total_input += e.fe_length;

		u64 src_max = roundup(src_size, block_bytes(c));

		e.fe_length = min(e.fe_length, src_max - e.fe_logical);

		unsigned visible_len = min(src_size - e.fe_logical, e.fe_length);

		if ((e.fe_logical	& (block_bytes(c) - 1)) ||
		    (e.fe_length	& (block_bytes(c) - 1)))
			die("Unaligned extent in %s - can't handle", src_path);

		if (BCH_MIGRATE_copy == s->type || (e.fe_flags & (FIEMAP_EXTENT_UNKNOWN|
				  FIEMAP_EXTENT_ENCODED|
				  FIEMAP_EXTENT_NOT_ALIGNED|
				  FIEMAP_EXTENT_DATA_INLINE))) {
			copy_data(c, dst, src_fd, e.fe_logical,
				  e.fe_logical + visible_len);
			s->total_wrote += visible_len;
			continue;
		}

		/* If the data is in bcachefs's superblock region, copy it: */
		if (e.fe_physical < s->reserve_start) {
			copy_data(c, dst, src_fd, e.fe_logical,
				  e.fe_logical + visible_len);
			s->total_wrote += visible_len;
			continue;
		}

		if ((e.fe_physical	& (block_bytes(c) - 1)))
			die("Unaligned extent in %s - can't handle", src_path);

		range_add(&s->extents, e.fe_physical, e.fe_length);
		link_data(c, dst, e.fe_logical, e.fe_physical, e.fe_length);
		s->total_linked += e.fe_length;
	}
	fiemap_iter_exit(&iter);
}

static struct range align_range(struct range r, unsigned bs)
{
	r.start	= round_down(r.start,	bs);
	r.end	= round_up(r.end,	bs);
	return r;
}

struct range seek_data(int fd, u64 i_size, loff_t o)
{
	s64 s = lseek(fd, o, SEEK_DATA);
	if (s < 0 && errno == ENXIO)
		return (struct range) {};
	if (s < 0)
		die("lseek error: %m");

	s64 e = lseek(fd, s, SEEK_HOLE);
	if (e < 0 && errno == ENXIO)
		e = i_size;
	if (e < 0)
		die("lseek error: %m");

	return (struct range) { s, e };
}

static struct range seek_data_aligned(int fd, u64 i_size, loff_t o, unsigned bs)
{
	struct range r = align_range(seek_data(fd, i_size, o), bs);
	if (!r.end)
		return r;

	while (true) {
		struct range n = align_range(seek_data(fd, i_size, r.end), bs);
		if (!n.end || r.end < n.start)
			break;

		r.end = n.end;
	}

	return r;
}

struct range seek_mismatch(const char *buf1, const char *buf2,
			   unsigned o, unsigned len)
{
	while (o < len && buf1[o] == buf2[o])
		o++;

	if (o == len)
		return (struct range) {};

	unsigned s = o;
	while (o < len && buf1[o] != buf2[o])
		o++;

	return (struct range) { s, o };
}

static struct range seek_mismatch_aligned(const char *buf1, const char *buf2,
					  unsigned offset, unsigned len,
					  unsigned bs)
{
	struct range r = align_range(seek_mismatch(buf1, buf2, offset, len), bs);
	if (r.end)
		while (true) {
			struct range n = align_range(seek_mismatch(buf1, buf2, r.end, len), bs);
			if (!n.end || r.end < n.start)
				break;

			r.end = n.end;
		}

	return r;
}

static void copy_sync_file_range(struct bch_fs *c,
				 struct copy_fs_state *s,
				 subvol_inum dst_inum,
				 struct bch_inode_unpacked *dst,
				 int src_fd, u64 src_size,
				 struct range r)
{
	while (r.start != r.end) {
		BUG_ON(r.start > r.end);

		unsigned b = min(r.end - r.start, WRITE_DATA_BUF);

		memset(src_buf, 0, b);
		xpread(src_fd, src_buf, min(b, src_size - r.start), r.start);

		read_data(c, dst_inum, dst, r.start, dst_buf, b);

		struct range m = {};
		while ((m = seek_mismatch_aligned(src_buf, dst_buf,
						  m.end, b, c->opts.block_size)).end) {
			write_data(c, dst, r.start + m.start,
				   src_buf + m.start, m.end - m.start);
			s->total_wrote += m.end - m.start;
		}

		r.start += b;
	}
}

static void copy_sync_file_data(struct bch_fs *c,
				struct copy_fs_state *s,
				subvol_inum dst_inum,
				struct bch_inode_unpacked *dst,
				int src_fd, u64 src_size)
{
	s64 i_sectors_delta = 0;

	struct range next, prev = {};

	while ((next = seek_data_aligned(src_fd, src_size, prev.end, c->opts.block_size)).end) {
		if (next.start) {
			BUG_ON(prev.end >= next.start);

			int ret = bch2_fpunch(c, dst_inum, prev.end >> 9, next.start >> 9, &i_sectors_delta);
			if (ret)
				die("bch2_fpunch error: %s", bch2_err_str(ret));
		}

		copy_sync_file_range(c, s, dst_inum, dst, src_fd, src_size, next);

		s->total_input += next.end - next.start;

		prev = next;
	}

	/* end of file, truncate remaining */
	int ret = bch2_fpunch(c, dst_inum, prev.end >> 9, U64_MAX, &i_sectors_delta);
	if (ret)
		die("bch2_fpunch error: %s", bch2_err_str(ret));
}

static int dirent_cmp(const void *_l, const void *_r)
{
	const struct dirent *l = _l;
	const struct dirent *r = _r;

	return  cmp_int(l->d_type, r->d_type) ?:
		strcmp(l->d_name, r->d_name);
}

typedef DARRAY(struct dirent) dirents;

struct readdir_out {
	struct dir_context	ctx;
	dirents			*dirents;
};

static int readdir_actor(struct dir_context *ctx, const char *name, int name_len,
			 loff_t pos, u64 inum, unsigned type)
{
	struct readdir_out *out = container_of(ctx, struct readdir_out, ctx);

	struct dirent d = {
		.d_ino	= inum,
		.d_type	= type,
	};
	memcpy(d.d_name, name, name_len);
	d.d_name[name_len] = '\0';

	return darray_push(out->dirents, d);
}

static int simple_readdir(struct bch_fs *c,
			  subvol_inum dir_inum,
			  struct bch_inode_unpacked *dir,
			  dirents *dirents)
{
	darray_init(dirents);

	struct bch_hash_info hash_info;
	try(bch2_hash_info_init(c, dir, &hash_info));
	struct readdir_out dst_dirents = { .ctx.actor = readdir_actor, .dirents = dirents };

	int ret = bch2_readdir(c, dir_inum, &hash_info, &dst_dirents.ctx);
	bch_err_fn(c, ret);
	if (ret) {
		darray_exit(dirents);
		return ret;
	}

	sort(dirents->data, dirents->nr, sizeof(dirents->data[0]), dirent_cmp, NULL);
	return 0;
}

static int recursive_remove(struct bch_fs *c,
			    subvol_inum dir_inum,
			    struct bch_inode_unpacked *dir,
			    struct dirent *d)
{
	subvol_inum child_inum = dir_inum;
	child_inum.inum = d->d_ino;

	struct bch_inode_unpacked child;
	int ret = bch2_inode_find_by_inum(c, child_inum, &child);
	bch_err_msg(c, ret, "looking up inode for %s", d->d_name);
	if (ret)
		return ret;

	if (S_ISDIR(child.bi_mode)) {
		dirents child_dirents;
		ret = simple_readdir(c, child_inum, &child, &child_dirents);
		if (ret)
			return ret;

		darray_for_each(child_dirents, i) {
			ret = recursive_remove(c, child_inum, &child, i);
			if (ret) {
				darray_exit(&child_dirents);
				return ret;
			}
		}

		darray_exit(&child_dirents);
	}

	return unlink_and_rm(c, dir_inum, dir, d->d_name);
}

static int delete_non_matching_dirents(struct bch_fs *c,
				       struct copy_fs_state *s,
				       subvol_inum dst_dir_inum,
				       struct bch_inode_unpacked *dst_dir,
				       dirents src_dirents)
{
	/* Assumes single subvolume */

	dirents dst_dirents;
	int ret = simple_readdir(c, dst_dir_inum, dst_dir, &dst_dirents);
	if (ret)
		return ret;

	struct dirent *src_d = src_dirents.data;
	darray_for_each(dst_dirents, dst_d) {
		while (src_d < &darray_top(src_dirents) &&
		       dirent_cmp(src_d, dst_d) < 0)
			src_d++;

		if (src_d == &darray_top(src_dirents) ||
		    dirent_cmp(src_d, dst_d)) {
			if (subvol_inum_eq(dst_dir_inum, BCACHEFS_ROOT_SUBVOL_INUM) &&
			    !strcmp(dst_d->d_name, "lost+found"))
				continue;

			if (s->verbosity > 1)
				printf("deleting %s\n", dst_d->d_name);

			ret = recursive_remove(c, dst_dir_inum, dst_dir, dst_d);
			if (ret)
				goto err;
		}
	}
err:
	darray_exit(&dst_dirents);
	return ret;
}

static int copy_dir(struct bch_fs *c,
		    struct copy_fs_state *s,
		    struct bch_inode_unpacked *dst,
		    int src_fd, const char *src_path)
{
	lseek(src_fd, 0, SEEK_SET);

	DIR *dir = fdopendir(src_fd);
	struct dirent *d;
	dirents dirents = {};

	while ((errno = 0), (d = readdir(dir)))
		darray_push(&dirents, *d);

	if (errno)
		die("readdir error: %m");

	sort(dirents.data, dirents.nr, sizeof(dirents.data[0]), dirent_cmp, NULL);

	subvol_inum dir_inum = { 1, dst->bi_inum };
	int ret = delete_non_matching_dirents(c, s, dir_inum, dst, dirents);
	if (ret)
		goto err;

	darray_for_each(dirents, d) {
		struct bch_inode_unpacked inode;
		int fd;

		if (fchdir(src_fd))
			die("fchdir error: %m");

		struct stat stat =
			xfstatat(src_fd, d->d_name, AT_SYMLINK_NOFOLLOW);

		if (!strcmp(d->d_name, ".") ||
		    !strcmp(d->d_name, "..") ||
		    !strcmp(d->d_name, "lost+found"))
			continue;

		if (BCH_MIGRATE_migrate == s->type && stat.st_ino == s->bcachefs_inum)
			continue;

		s->total_files++;

		char *child_path = mprintf("%s/%s", src_path, d->d_name);

		if (s->type == BCH_MIGRATE_migrate && stat.st_dev != s->dev)
			die("%s does not have correct st_dev!", child_path);

		struct hardlink *h = NULL;
		if (S_ISREG(stat.st_mode) && stat.st_nlink > 1) {
			u64 ino = stat.st_ino;
			h = rhashtable_lookup_fast(&s->hardlinks, &ino, hardlink_params);
			if (!h) {
				h = kzalloc(sizeof(*h), GFP_KERNEL);
				h->src = ino;
				int ret = rhashtable_lookup_insert_fast(&s->hardlinks, &h->hash,
									hardlink_params);
				BUG_ON(ret);
			}
		}

		subvol_inum dst_dir_inum = { 1, dst->bi_inum };

		if (h && h->dst) {
			ret = create_or_update_link(c, dst_dir_inum, dst, d->d_name,
						    (subvol_inum) { 1, h->dst}, S_IFREG);
			if (ret)
				goto err;
			goto next;
		}

		inode = create_or_update_file(c, dst_dir_inum, dst, d->d_name,
				    stat.st_uid, stat.st_gid,
				    stat.st_mode, stat.st_rdev);

		subvol_inum dst_child_inum = { 1, inode.bi_inum };

		if (h)
			h->dst = inode.bi_inum;

		copy_xattrs(c, &inode, d->d_name);

		switch (mode_to_type(stat.st_mode)) {
		case DT_DIR:
			fd = xopen(d->d_name, O_RDONLY|O_NOATIME);
			ret = copy_dir(c, s, &inode, fd, child_path);
			if (ret)
				goto err;
			break;
		case DT_REG:
			inode.bi_size = stat.st_size;

			fd = xopen(d->d_name, O_RDONLY|O_NOATIME);
			if (s->type == BCH_MIGRATE_migrate)
				link_file_data(c, s, &inode,
					       fd, child_path, stat.st_size);
			else
				copy_sync_file_data(c, s, dst_child_inum, &inode,
						    fd, stat.st_size);
			xclose(fd);
			break;
		case DT_LNK:
			inode.bi_size = stat.st_size;

			copy_link(c, dst_child_inum, &inode, d->d_name);
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
err:
	darray_exit(&dirents);
	closedir(dir);
	return ret;
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

	subvol_inum root_inum = { 1, root_inode->bi_inum };
	dst = create_or_update_file(c, root_inum, root_inode,
			  "old_migrated_filesystem",
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

int copy_fs(struct bch_fs *c, struct copy_fs_state *s,
	    int src_fd, const char *src_path)
{
	if (!S_ISDIR(xfstat(src_fd).st_mode))
		die("%s is not a directory", src_path);

	if (s->type == BCH_MIGRATE_migrate)
		syncfs(src_fd);

	BUG_ON(rhashtable_init(&s->hardlinks, &hardlink_params));

	struct bch_inode_unpacked root_inode;
	int ret = bch2_inode_find_by_inum(c, (subvol_inum) { 1, BCACHEFS_ROOT_INO },
					  &root_inode);
	bch_err_msg(c, ret, "looking up root directory");
	if (ret)
		return ret;

	if (fchdir(src_fd))
		die("fchdir error: %m");

	struct stat stat = xfstat(src_fd);
	copy_times(c, &root_inode, &stat);
	copy_xattrs(c, &root_inode, ".");

	/* now, copy: */
	ret = copy_dir(c, s, &root_inode, dup(src_fd), src_path);
	bch_err_msg(c, ret, "copying filesystem");
	if (ret)
		return ret;

	if (s->type == BCH_MIGRATE_migrate)
		reserve_old_fs_space(c, &root_inode, &s->extents, s->reserve_start);

	update_inode(c, &root_inode);

	darray_exit(&s->extents);
	/*
	 * We're currently leaking s->hardlinks: we want to convert this back to
	 * a radix tree, when we have a radix tree that supports real 64 bit
	 * integer keys
	 */
	//genradix_free(&s->hardlinks);

	CLASS(printbuf, buf)();
	printbuf_tabstop_start(&buf);
	prt_printf(&buf, "Total files:\t%llu", s->total_files);
	prt_newline(&buf);
	prt_str_indented(&buf, "Total input:\t");
	prt_human_readable_u64(&buf, s->total_input);
	prt_newline(&buf);

	if (s->total_wrote) {
		prt_str(&buf, "Wrote:\t");
		prt_human_readable_u64(&buf, s->total_wrote);
		prt_newline(&buf);
	}

	if (s->total_linked) {
		prt_str(&buf, "Linked:\t");
		prt_human_readable_u64(&buf, s->total_linked);
		prt_newline(&buf);
	}

	prt_newline(&buf);
	printbuf_tabstop_finish(&buf);

	fputs(buf.buf, stdout);
	return 0;
}
