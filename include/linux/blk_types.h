/*
 * Block data types and constants.  Directly include this file only to
 * break include dependency loop.
 */
#ifndef __LINUX_BLK_TYPES_H
#define __LINUX_BLK_TYPES_H

#include <linux/atomic.h>
#include <linux/backing-dev.h>
#include <linux/types.h>
#include <linux/bvec.h>
#include <linux/kobject.h>
#include <linux/mutex.h>
#include <linux/rwsem.h>

struct bio_set;
struct bio;
typedef void (bio_end_io_t) (struct bio *);

#define BDEVNAME_SIZE	32

typedef unsigned int __bitwise blk_mode_t;

/* open for reading */
#define BLK_OPEN_READ		((__force blk_mode_t)(1 << 0))
/* open for writing */
#define BLK_OPEN_WRITE		((__force blk_mode_t)(1 << 1))
/* open exclusively (vs other exclusive openers */
#define BLK_OPEN_EXCL		((__force blk_mode_t)(1 << 2))
/* opened with O_NDELAY */
#define BLK_OPEN_NDELAY		((__force blk_mode_t)(1 << 3))
/* open for "writes" only for ioctls (specialy hack for floppy.c) */
#define BLK_OPEN_WRITE_IOCTL	((__force blk_mode_t)(1 << 4))

#define BLK_OPEN_BUFFERED	((__force blk_mode_t)(1 << 5))
#define BLK_OPEN_CREAT		((__force blk_mode_t)(1 << 6))

struct inode {
	unsigned long		i_ino;
	loff_t			i_size;
	struct super_block	*i_sb;
	blk_mode_t		mode;
	void			*i_private;
};

struct request_queue {
	struct backing_dev_info *backing_dev_info;
};

struct gendisk {
	struct backing_dev_info	*bdi;
	struct backing_dev_info	__bdi;
};

struct hd_struct {
	struct kobject		kobj;
};

struct block_device {
	struct kobject		kobj;

	struct			{
		struct kobject	kobj;
	}			bd_device;

	dev_t			bd_dev;
	char			name[BDEVNAME_SIZE];
	struct inode		*bd_inode;
	struct inode		__bd_inode;
	struct request_queue	queue;
	void			*bd_holder;
	struct gendisk *	bd_disk;
	struct gendisk		__bd_disk;
	int			bd_fd;

	struct mutex		bd_holder_lock;
};

#define bdev_kobj(_bdev) (&((_bdev)->kobj))

/*
 * Block error status values.  See block/blk-core:blk_errors for the details.
 */
typedef u8 __bitwise blk_status_t;
#define	BLK_STS_OK 0
#define BLK_STS_NOTSUPP		((__force blk_status_t)1)
#define BLK_STS_TIMEOUT		((__force blk_status_t)2)
#define BLK_STS_NOSPC		((__force blk_status_t)3)
#define BLK_STS_TRANSPORT	((__force blk_status_t)4)
#define BLK_STS_TARGET		((__force blk_status_t)5)
#define BLK_STS_RESV_CONFLICT	((__force blk_status_t)6)
#define BLK_STS_MEDIUM		((__force blk_status_t)7)
#define BLK_STS_PROTECTION	((__force blk_status_t)8)
#define BLK_STS_RESOURCE	((__force blk_status_t)9)
#define BLK_STS_IOERR		((__force blk_status_t)10)

/* hack for device mapper, don't use elsewhere: */
#define BLK_STS_DM_REQUEUE    ((__force blk_status_t)11)

#define BLK_STS_AGAIN		((__force blk_status_t)12)
#define BLK_STS_DEV_RESOURCE	((__force blk_status_t)13)
#define BLK_STS_ZONE_OPEN_RESOURCE	((__force blk_status_t)14)
#define BLK_STS_ZONE_ACTIVE_RESOURCE	((__force blk_status_t)15)
#define BLK_STS_OFFLINE		((__force blk_status_t)16)
#define BLK_STS_DURATION_LIMIT	((__force blk_status_t)17)
#define BLK_STS_INVAL	((__force blk_status_t)19)

#define BIO_INLINE_VECS 4

/*
 * main unit of I/O for the block layer and lower layers (ie drivers and
 * stacking drivers)
 */
struct bio {
	struct bio		*bi_next;	/* request queue link */
	struct block_device	*bi_bdev;
	blk_status_t		bi_status;
	unsigned int		bi_opf;		/* bottom bits req flags,
						 * top bits REQ_OP. Use
						 * accessors.
						 */
	unsigned short		bi_flags;	/* status, command, etc */
	unsigned short		bi_ioprio;

	struct bvec_iter	bi_iter;

	atomic_t		__bi_remaining;

	bio_end_io_t		*bi_end_io;
	void			*bi_private;

	unsigned short		bi_vcnt;	/* how many bio_vec's */

	/*
	 * Everything starting with bi_max_vecs will be preserved by bio_reset()
	 */

	unsigned short		bi_max_vecs;	/* max bvl_vecs we can hold */

	atomic_t		__bi_cnt;	/* pin count */

	struct bio_vec		*bi_io_vec;	/* the actual vec list */

	struct bio_set		*bi_pool;
};

static inline struct bio_vec *bio_inline_vecs(struct bio *bio)
{
	return (struct bio_vec *)(bio + 1);
}

#define BIO_RESET_BYTES		offsetof(struct bio, bi_max_vecs)

/*
 * bio flags
 */
#define BIO_SEG_VALID	1	/* bi_phys_segments valid */
#define BIO_CLONED	2	/* doesn't own data */
#define BIO_BOUNCED	3	/* bio is a bounce bio */
#define BIO_USER_MAPPED 4	/* contains user pages */
#define BIO_NULL_MAPPED 5	/* contains invalid user pages */
#define BIO_QUIET	6	/* Make BIO Quiet */
#define BIO_CHAIN	7	/* chained bio, ->bi_remaining in effect */
#define BIO_REFFED	8	/* bio has elevated ->bi_cnt */

/*
 * Flags starting here get preserved by bio_reset() - this includes
 * BVEC_POOL_IDX()
 */
#define BIO_RESET_BITS	10

/*
 * We support 6 different bvec pools, the last one is magic in that it
 * is backed by a mempool.
 */
#define BVEC_POOL_NR		6
#define BVEC_POOL_MAX		(BVEC_POOL_NR - 1)

/*
 * Top 4 bits of bio flags indicate the pool the bvecs came from.  We add
 * 1 to the actual index so that 0 indicates that there are no bvecs to be
 * freed.
 */
#define BVEC_POOL_BITS		(4)
#define BVEC_POOL_OFFSET	(16 - BVEC_POOL_BITS)
#define BVEC_POOL_IDX(bio)	((bio)->bi_flags >> BVEC_POOL_OFFSET)

/*
 * Operations and flags common to the bio and request structures.
 * We use 8 bits for encoding the operation, and the remaining 24 for flags.
 *
 * The least significant bit of the operation number indicates the data
 * transfer direction:
 *
 *   - if the least significant bit is set transfers are TO the device
 *   - if the least significant bit is not set transfers are FROM the device
 *
 * If a operation does not transfer data the least significant bit has no
 * meaning.
 */
#define REQ_OP_BITS	8
#define REQ_OP_MASK	((1 << REQ_OP_BITS) - 1)
#define REQ_FLAG_BITS	24

typedef __u32 __bitwise blk_opf_t;

enum req_op {
	/* read sectors from the device */
	REQ_OP_READ		= (__force blk_opf_t)0,
	/* write sectors to the device */
	REQ_OP_WRITE		= (__force blk_opf_t)1,
	/* flush the volatile write cache */
	REQ_OP_FLUSH		= (__force blk_opf_t)2,
	/* discard sectors */
	REQ_OP_DISCARD		= (__force blk_opf_t)3,
	/* securely erase sectors */
	REQ_OP_SECURE_ERASE	= (__force blk_opf_t)5,
	/* write data at the current zone write pointer */
	REQ_OP_ZONE_APPEND	= (__force blk_opf_t)7,
	/* write the zero filled sector many times */
	REQ_OP_WRITE_ZEROES	= (__force blk_opf_t)9,
	/* Open a zone */
	REQ_OP_ZONE_OPEN	= (__force blk_opf_t)10,
	/* Close a zone */
	REQ_OP_ZONE_CLOSE	= (__force blk_opf_t)11,
	/* Transition a zone to full */
	REQ_OP_ZONE_FINISH	= (__force blk_opf_t)13,
	/* reset a zone write pointer */
	REQ_OP_ZONE_RESET	= (__force blk_opf_t)15,
	/* reset all the zone present on the device */
	REQ_OP_ZONE_RESET_ALL	= (__force blk_opf_t)17,

	/* Driver private requests */
	REQ_OP_DRV_IN		= (__force blk_opf_t)34,
	REQ_OP_DRV_OUT		= (__force blk_opf_t)35,

	REQ_OP_LAST		= (__force blk_opf_t)36,
};

/* Keep cmd_flag_name[] in sync with the definitions below */
enum req_flag_bits {
	__REQ_FAILFAST_DEV =	/* no driver retries of device errors */
		REQ_OP_BITS,
	__REQ_FAILFAST_TRANSPORT, /* no driver retries of transport errors */
	__REQ_FAILFAST_DRIVER,	/* no driver retries of driver errors */
	__REQ_SYNC,		/* request is sync (sync write or read) */
	__REQ_META,		/* metadata io request */
	__REQ_PRIO,		/* boost priority in cfq */
	__REQ_NOMERGE,		/* don't touch this for merging */
	__REQ_IDLE,		/* anticipate more IO after this one */
	__REQ_INTEGRITY,	/* I/O includes block integrity payload */
	__REQ_FUA,		/* forced unit access */
	__REQ_PREFLUSH,		/* request for cache flush */
	__REQ_RAHEAD,		/* read ahead, can fail anytime */
	__REQ_BACKGROUND,	/* background IO */
	__REQ_NOWAIT,           /* Don't wait if request will block */
	__REQ_POLLED,		/* caller polls for completion using bio_poll */
	__REQ_ALLOC_CACHE,	/* allocate IO from cache if available */
	__REQ_SWAP,		/* swap I/O */
	__REQ_DRV,		/* for driver use */
	__REQ_FS_PRIVATE,	/* for file system (submitter) use */
	__REQ_ATOMIC,		/* for atomic write operations */
	__REQ_P2PDMA,		/* contains P2P DMA pages */
	/*
	 * Command specific flags, keep last:
	 */
	/* for REQ_OP_WRITE_ZEROES: */
	__REQ_NOUNMAP,		/* do not free blocks when zeroing */

	__REQ_NR_BITS,		/* stops here */
};

#define REQ_FAILFAST_DEV	\
			(__force blk_opf_t)(1ULL << __REQ_FAILFAST_DEV)
#define REQ_FAILFAST_TRANSPORT	\
			(__force blk_opf_t)(1ULL << __REQ_FAILFAST_TRANSPORT)
#define REQ_FAILFAST_DRIVER	\
			(__force blk_opf_t)(1ULL << __REQ_FAILFAST_DRIVER)
#define REQ_SYNC	(__force blk_opf_t)(1ULL << __REQ_SYNC)
#define REQ_META	(__force blk_opf_t)(1ULL << __REQ_META)
#define REQ_PRIO	(__force blk_opf_t)(1ULL << __REQ_PRIO)
#define REQ_NOMERGE	(__force blk_opf_t)(1ULL << __REQ_NOMERGE)
#define REQ_IDLE	(__force blk_opf_t)(1ULL << __REQ_IDLE)
#define REQ_INTEGRITY	(__force blk_opf_t)(1ULL << __REQ_INTEGRITY)
#define REQ_FUA		(__force blk_opf_t)(1ULL << __REQ_FUA)
#define REQ_PREFLUSH	(__force blk_opf_t)(1ULL << __REQ_PREFLUSH)
#define REQ_RAHEAD	(__force blk_opf_t)(1ULL << __REQ_RAHEAD)
#define REQ_BACKGROUND	(__force blk_opf_t)(1ULL << __REQ_BACKGROUND)
#define REQ_NOWAIT	(__force blk_opf_t)(1ULL << __REQ_NOWAIT)
#define REQ_POLLED	(__force blk_opf_t)(1ULL << __REQ_POLLED)
#define REQ_ALLOC_CACHE	(__force blk_opf_t)(1ULL << __REQ_ALLOC_CACHE)
#define REQ_SWAP	(__force blk_opf_t)(1ULL << __REQ_SWAP)
#define REQ_DRV		(__force blk_opf_t)(1ULL << __REQ_DRV)
#define REQ_FS_PRIVATE	(__force blk_opf_t)(1ULL << __REQ_FS_PRIVATE)
#define REQ_ATOMIC	(__force blk_opf_t)(1ULL << __REQ_ATOMIC)
#define REQ_P2PDMA	(__force blk_opf_t)(1ULL << __REQ_P2PDMA)

#define REQ_NOUNMAP	(__force blk_opf_t)(1ULL << __REQ_NOUNMAP)

#define REQ_NOMERGE_FLAGS	(REQ_PREFLUSH | REQ_FUA)

#define bio_op(bio) \
	((bio)->bi_opf & REQ_OP_MASK)

static inline void bio_set_op_attrs(struct bio *bio, unsigned op,
		unsigned op_flags)
{
	bio->bi_opf = op | op_flags;
}

#define RW_MASK			REQ_OP_WRITE

#define READ			REQ_OP_READ
#define WRITE			REQ_OP_WRITE

#define READ_SYNC		REQ_SYNC
#define WRITE_SYNC		(REQ_SYNC)
#define WRITE_ODIRECT		REQ_SYNC
#define WRITE_FLUSH		(REQ_SYNC | REQ_PREFLUSH)
#define WRITE_FUA		(REQ_SYNC | REQ_FUA)
#define WRITE_FLUSH_FUA		(REQ_SYNC | REQ_PREFLUSH | REQ_FUA)

#endif /* __LINUX_BLK_TYPES_H */
