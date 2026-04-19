#ifndef __TOOLS_LINUX_BLKDEV_H
#define __TOOLS_LINUX_BLKDEV_H

#include <linux/backing-dev.h>
#include <linux/blk_types.h>
#include <linux/kobject.h>
#include <linux/types.h>

#define MAX_LFS_FILESIZE 	((loff_t)LLONG_MAX)

#define BIO_MAX_VECS	256U

typedef unsigned fmode_t;
typedef __u32 __bitwise blk_opf_t;

struct bio;
struct user_namespace;

#define MINORBITS	20
#define MINORMASK	((1U << MINORBITS) - 1)

#define MAJOR(dev)	((unsigned int) ((dev) >> MINORBITS))
#define MINOR(dev)	((unsigned int) ((dev) & MINORMASK))
#define MKDEV(ma,mi)	(((ma) << MINORBITS) | (mi))

struct file {
	struct inode		*f_inode;
	void			*private_data;
};

static inline struct inode *file_inode(const struct file *f)
{
	return f->f_inode;
}

#define part_to_dev(part)	(part)

void generic_make_request(struct bio *);
int submit_bio_wait(struct bio *);

static inline void submit_bio(struct bio *bio)
{
	generic_make_request(bio);
}

int blkdev_issue_discard(struct block_device *, sector_t, sector_t, gfp_t);
int blkdev_issue_zeroout(struct block_device *, sector_t, sector_t, gfp_t, unsigned);

#define bdev_get_queue(bdev)		(&((bdev)->queue))

#ifndef SECTOR_SHIFT
#define SECTOR_SHIFT 9
#endif
#ifndef SECTOR_SIZE
#define SECTOR_SIZE (1 << SECTOR_SHIFT)
#endif

#define PAGE_SECTORS_SHIFT	(PAGE_SHIFT - SECTOR_SHIFT)
#define PAGE_SECTORS		(1 << PAGE_SECTORS_SHIFT)
#define SECTOR_MASK		(PAGE_SECTORS - 1)

#define bdev_max_discard_sectors(bdev)	((void) (bdev), 0)
#define blk_queue_nonrot(q)		((void) (q), 0)

struct blk_plug {
};

static inline void blk_start_plug(struct blk_plug *plug) {}
static inline void blk_finish_plug(struct blk_plug *plug) {}

unsigned bdev_logical_block_size(struct block_device *bdev);
bool bdev_nonrot(struct block_device *);
sector_t get_capacity(struct gendisk *disk);

struct blk_holder_ops {
        void (*mark_dead)(struct block_device *bdev, bool surprise);
	void (*sync)(struct block_device *bdev);
	int (*freeze)(struct block_device *bdev);
	int (*thaw)(struct block_device *bdev);
};

static inline struct block_device *file_bdev(struct file *file)
{
	return container_of(file->f_inode, struct block_device, __bd_inode);
}

void bdev_fput(struct file *);
struct file *bdev_file_open_by_path(const char *, blk_mode_t, void *,
				    const struct blk_holder_ops *);
int lookup_bdev(const char *path, dev_t *);

struct super_block {
	void			*s_fs_info;
	struct rw_semaphore	s_umount;
};

static inline void evict_inodes(struct super_block *sb) {}
static inline int sync_filesystem(struct super_block *sb) { return 0; }

/*
 * File types
 *
 * NOTE! These match bits 12..15 of stat.st_mode
 * (ie "(i_mode >> 12) & 15").
 */
#ifndef DT_UNKNOWN
#define DT_UNKNOWN	0
#define DT_FIFO		1
#define DT_CHR		2
#define DT_DIR		4
#define DT_BLK		6
#define DT_REG		8
#define DT_LNK		10
#define DT_SOCK		12
#define DT_WHT		14
#define DT_MAX		16
#endif

/*
 * This is the "filldir" function type, used by readdir() to let
 * the kernel specify what kind of dirent layout it wants to have.
 * This allows the kernel to read directories into kernel space or
 * to have different dirent layouts depending on the binary type.
 */
struct dir_context;
typedef int (*filldir_t)(struct dir_context *, const char *, int, loff_t, u64,
			 unsigned);

struct dir_context {
	const filldir_t actor;
	u64 pos;
};

struct file_operations {
	struct module *owner;
	int (*open) (struct inode *, struct file *);
	int (*release) (struct inode *, struct file *);
	ssize_t (*read) (struct file *, char __user *, size_t, loff_t *);
};

static inline int register_chrdev(unsigned int major, const char *name,
				  const struct file_operations *fops)
{
	return 1;
}

static inline void unregister_chrdev(unsigned int major, const char *name)
{
}

static inline const char *bdevname(struct block_device *bdev, char *buf)
{
	snprintf(buf, BDEVNAME_SIZE, "%s", bdev->name);
	return buf;
}

static inline bool op_is_write(unsigned int op)
{
	return op == REQ_OP_READ ? false : true;
}

/*
 * return data direction, READ or WRITE
 */
static inline int bio_data_dir(struct bio *bio)
{
	return op_is_write(bio_op(bio)) ? WRITE : READ;
}

static inline bool dir_emit(struct dir_context *ctx,
			    const char *name, int namelen,
			    u64 ino, unsigned type)
{
	return ctx->actor(ctx, name, namelen, ctx->pos, ino, type) == 0;
}

static inline bool dir_emit_dots(struct file *file, struct dir_context *ctx)
{
	return true;
}

#define capable(cap)		true

int blk_status_to_errno(blk_status_t status);
blk_status_t errno_to_blk_status(int errno);
const char *blk_status_to_str(blk_status_t status);

static inline void invalidate_bdev(struct block_device *bdev) {}

void blkdev_init(void);

#endif /* __TOOLS_LINUX_BLKDEV_H */

