#ifndef __TOOLS_LINUX_BLKDEV_H
#define __TOOLS_LINUX_BLKDEV_H

#include <linux/backing-dev.h>
#include <linux/blk_types.h>
#include <linux/kobject.h>
#include <linux/types.h>

#define BIO_MAX_VECS	256

typedef unsigned fmode_t;

struct bio;
struct user_namespace;

#define MINORBITS	20
#define MINORMASK	((1U << MINORBITS) - 1)

#define MAJOR(dev)	((unsigned int) ((dev) >> MINORBITS))
#define MINOR(dev)	((unsigned int) ((dev) & MINORMASK))
#define MKDEV(ma,mi)	(((ma) << MINORBITS) | (mi))

/* file is open for reading */
#define FMODE_READ		((__force fmode_t)0x1)
/* file is open for writing */
#define FMODE_WRITE		((__force fmode_t)0x2)
/* file is seekable */
#define FMODE_LSEEK		((__force fmode_t)0x4)
/* file can be accessed using pread */
#define FMODE_PREAD		((__force fmode_t)0x8)
/* file can be accessed using pwrite */
#define FMODE_PWRITE		((__force fmode_t)0x10)
/* File is opened for execution with sys_execve / sys_uselib */
#define FMODE_EXEC		((__force fmode_t)0x20)
/* File is opened with O_NDELAY (only set for block devices) */
#define FMODE_NDELAY		((__force fmode_t)0x40)
/* File is opened with O_EXCL (only set for block devices) */
#define FMODE_EXCL		((__force fmode_t)0x80)
/* File is opened using open(.., 3, ..) and is writeable only for ioctls
   (specialy hack for floppy.c) */
#define FMODE_WRITE_IOCTL	((__force fmode_t)0x100)
/* 32bit hashes as llseek() offset (for directories) */
#define FMODE_32BITHASH         ((__force fmode_t)0x200)
/* 64bit hashes as llseek() offset (for directories) */
#define FMODE_64BITHASH         ((__force fmode_t)0x400)

struct inode {
	unsigned long		i_ino;
	loff_t			i_size;
	struct super_block	*i_sb;
};

struct file {
	struct inode		*f_inode;
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

int blkdev_issue_discard(struct block_device *, sector_t,
			 sector_t, gfp_t, unsigned long);

#define bdev_get_queue(bdev)		(&((bdev)->queue))

#define blk_queue_discard(q)		((void) (q), 0)
#define blk_queue_nonrot(q)		((void) (q), 0)

unsigned bdev_logical_block_size(struct block_device *bdev);
sector_t get_capacity(struct gendisk *disk);

void blkdev_put(struct block_device *bdev, fmode_t mode);
void bdput(struct block_device *bdev);
struct block_device *blkdev_get_by_path(const char *path, fmode_t mode, void *holder);
int lookup_bdev(const char *path, dev_t *);

struct super_block {
	void			*s_fs_info;
};

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

/* /sys/fs */
extern struct kobject *fs_kobj;

struct file_operations {
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

#endif /* __TOOLS_LINUX_BLKDEV_H */

