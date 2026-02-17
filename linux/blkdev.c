
#include <alloca.h>
#include <errno.h>
#include <fcntl.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/types.h>
#include <sys/uio.h>
#include <unistd.h>

#include <libaio.h>

#ifdef CONFIG_VALGRIND
#include <valgrind/memcheck.h>
#endif

#include <linux/bio.h>
#include <linux/blkdev.h>
#include <linux/completion.h>
#include <linux/fs.h>
#include <linux/kthread.h>

#include "tools-util.h"

struct fops {
	void (*init)(void);
	void (*cleanup)(void);
	void (*read)(struct bio *bio, struct iovec * iov, unsigned i);
	void (*write)(struct bio *bio, struct iovec * iov, unsigned i);
};

static void sync_check(struct bio *bio, int ret)
{
	if (ret != bio->bi_iter.bi_size) {
		fprintf(stderr, "IO error: %s\n", ret < 0 ? strerror(errno) : "short IO");
		bio->bi_status = BLK_STS_IOERR;
	}

	if (bio->bi_opf & REQ_FUA) {
		ret = fdatasync(bio->bi_bdev->bd_fd);
		if (ret) {
			fprintf(stderr, "fsync error: %s\n", strerror(errno));
			bio->bi_status = BLK_STS_IOERR;
		}
	}
	bio_endio(bio);
}

static void sync_init(void) {}
static void sync_cleanup(void) {}

static void sync_read(struct bio *bio, struct iovec * iov, unsigned i)
{

	ssize_t ret = preadv(bio->bi_bdev->bd_fd, iov, i,
			     bio->bi_iter.bi_sector << 9);
	sync_check(bio, ret);
}

static void sync_write(struct bio *bio, struct iovec * iov, unsigned i)
{
	ssize_t ret = pwritev2(bio->bi_bdev->bd_fd, iov, i,
			       bio->bi_iter.bi_sector << 9,
			       bio->bi_opf & REQ_FUA ? RWF_SYNC : 0);
	sync_check(bio, ret);
}

static void aio_init(void);
static void aio_cleanup(void);
static void aio_read(struct bio *bio, struct iovec *iov, unsigned i);
static void aio_write(struct bio *bio, struct iovec * iov, unsigned i);

struct fops fops_list[] = {
	{
		.init		= aio_init,
		.cleanup	= aio_cleanup,
		.read		= aio_read,
		.write		= aio_write,
	}, {
		.init		= sync_init,
		.cleanup	= sync_cleanup,
		.read		= sync_read,
		.write		= sync_write,
	}, {
		/* NULL */
	}
};

/* Use sync before we init threads */
static struct fops *fops = &fops_list[1];
static io_context_t aio_ctx;
static atomic_t running_requests;

void generic_make_request(struct bio *bio)
{
	struct iovec *iov;
	struct bvec_iter iter;
	struct bio_vec bv;
	ssize_t ret;
	unsigned i;

	if (bio->bi_opf & REQ_PREFLUSH) {
		ret = fdatasync(bio->bi_bdev->bd_fd);
		if (ret) {
			fprintf(stderr, "fsync error: %m\n");
			bio->bi_status = BLK_STS_IOERR;
			bio_endio(bio);
			return;
		}
	}

	i = 0;
	bio_for_each_segment(bv, bio, iter)
		i++;

	iov = alloca(sizeof(*iov) * i);

	i = 0;
	bio_for_each_segment(bv, bio, iter) {
		iov[i++] = (struct iovec) {
			.iov_base = bv.bv_addr,
			.iov_len = bv.bv_len,
		};

#ifdef CONFIG_VALGRIND
		/* To be pedantic it should only be on IO completion. */
		if (bio_op(bio) == REQ_OP_READ)
			VALGRIND_MAKE_MEM_DEFINED(bv.bv_addr, bv.bv_len);
#endif
	}

	switch (bio_op(bio)) {
	case REQ_OP_READ:
		fops->read(bio, iov, i);
		break;
	case REQ_OP_WRITE:
		fops->write(bio, iov, i);
		break;
	case REQ_OP_FLUSH:
		ret = fsync(bio->bi_bdev->bd_fd);
		if (ret)
			die("fsync error: %m");
		bio_endio(bio);
		break;
	default:
		BUG();
	}
}

static void submit_bio_wait_endio(struct bio *bio)
{
	complete(bio->bi_private);
}

int submit_bio_wait(struct bio *bio)
{
	struct completion done;

	init_completion(&done);
	bio->bi_private = &done;
	bio->bi_end_io = submit_bio_wait_endio;
	bio->bi_opf |= REQ_SYNC;
	submit_bio(bio);
	wait_for_completion(&done);

	return blk_status_to_errno(bio->bi_status);
}

int blkdev_issue_discard(struct block_device *bdev,
			 sector_t sector, sector_t nr_sects,
			 gfp_t gfp_mask)
{
	return 0;
}

int blkdev_issue_zeroout(struct block_device *bdev,
			 sector_t sector, sector_t nr_sects,
			 gfp_t gfp_mask, unsigned flags)
{
	/* Not yet implemented: */
	BUG();
}

unsigned bdev_logical_block_size(struct block_device *bdev)
{
	struct stat statbuf = xfstat(bdev->bd_fd);

	if (!S_ISBLK(statbuf.st_mode))
		return statbuf.st_blksize;

	unsigned blksize;
	xioctl(bdev->bd_fd, BLKSSZGET, &blksize);
	return blksize;
}

bool bdev_nonrot(struct block_device *bdev)
{
	struct stat statbuf = xfstat(bdev->bd_fd);
	if (!S_ISBLK(statbuf.st_mode))
		return false;

	char *path = mprintf("/sys/dev/block/%u:%u/queue/rotational",
			     major(statbuf.st_rdev),
			     minor(statbuf.st_rdev));
	u64 v = !access(path, R_OK)
		? read_file_u64(AT_FDCWD, path)
		: 0;
	free(path);
	return !v;
}

sector_t get_capacity(struct gendisk *disk)
{
	struct block_device *bdev =
		container_of(disk, struct block_device, __bd_disk);
	struct stat statbuf;
	u64 bytes;
	int ret;

	ret = fstat(bdev->bd_fd, &statbuf);
	BUG_ON(ret);

	if (!S_ISBLK(statbuf.st_mode))
		return statbuf.st_size >> 9;

	ret = ioctl(bdev->bd_fd, BLKGETSIZE64, &bytes);
	BUG_ON(ret);

	return bytes >> 9;
}

void bdev_fput(struct file *file)
{
	struct block_device *bdev = file_bdev(file);

	fdatasync(bdev->bd_fd);
	close(bdev->bd_fd);
	free(bdev);
	free(file);
}

struct file *bdev_file_open_by_path(const char *path, blk_mode_t mode,
				    void *holder, const struct blk_holder_ops *hop)
{
	int fd, flags = 0;

	if ((mode & (BLK_OPEN_READ|BLK_OPEN_WRITE)) == (BLK_OPEN_READ|BLK_OPEN_WRITE))
		flags = O_RDWR;
	else if (mode & BLK_OPEN_READ)
		flags = O_RDONLY;
	else if (mode & BLK_OPEN_WRITE)
		flags = O_WRONLY;

	if (!(mode & BLK_OPEN_BUFFERED))
		flags |= O_DIRECT;

	if (mode & BLK_OPEN_EXCL)
		flags |= O_EXCL;

	if (mode & BLK_OPEN_CREAT)
		flags |= O_CREAT;

	fd = open(path, flags, 0600);
	if (fd < 0)
		return ERR_PTR(-errno);

	struct block_device *bdev = malloc(sizeof(*bdev));
	memset(bdev, 0, sizeof(*bdev));

	/* strip directory */

	char *fname = strrchr(path, '/');

	strncpy(bdev->name, fname ? fname + 1 : path, sizeof(bdev->name));
	bdev->name[sizeof(bdev->name) - 1] = '\0';

	bdev->bd_dev		= xfstat(fd).st_rdev;
	bdev->bd_fd		= fd;
	bdev->bd_holder		= holder;
	bdev->bd_disk		= &bdev->__bd_disk;
	bdev->bd_disk->bdi	= &bdev->bd_disk->__bdi;
	bdev->queue.backing_dev_info = bdev->bd_disk->bdi;
	bdev->bd_inode		= &bdev->__bd_inode;

	mutex_init(&bdev->bd_holder_lock);

	struct file *file = calloc(sizeof(*file), 1);
	file->f_inode = bdev->bd_inode;

	return file;
}

int lookup_bdev(const char *path, dev_t *dev)
{
	return -EINVAL;
}

static void io_fallback(void)
{
	fops++;
	if (fops->init == NULL)
		die("no fallback possible, something is very wrong");
	fops->init();
}

static DECLARE_WAIT_QUEUE_HEAD(aio_events_completed);

static int aio_completion_thread(void *arg)
{
	struct io_event events[8], *ev;
	int ret;
	bool stop = false;

	while (!stop) {
		ret = io_getevents(aio_ctx, 1, ARRAY_SIZE(events),
				   events, NULL);

		if (ret < 0 && ret == -EINTR)
			continue;
		if (ret < 0)
			die("io_getevents() error: %s", strerror(-ret));
		if (ret)
			wake_up(&aio_events_completed);

		for (ev = events; ev < events + ret; ev++) {
			struct bio *bio = (struct bio *) ev->data;

			/* This should only happen during blkdev_cleanup() */
			if (!bio) {
				BUG_ON(atomic_read(&running_requests) != 0);
				stop = true;
				continue;
			}

			if (ev->res != bio->bi_iter.bi_size)
				bio->bi_status = BLK_STS_IOERR;

			bio_endio(bio);
			atomic_dec(&running_requests);
		}
	}

	return 0;
}

static struct task_struct *aio_task = NULL;

static void aio_init(void)
{
	struct task_struct *p;
	long err = io_setup(256, &aio_ctx);
	if (!err) {
		p = kthread_run(aio_completion_thread, NULL, "aio_completion");
		BUG_ON(IS_ERR(p));

		aio_task = p;

	} else if (err == -ENOSYS) {
		io_fallback();
	} else {
		die("io_setup() error: %s", strerror(err));
	}
}

static void aio_cleanup(void)
{
	struct task_struct *p = NULL;
	swap(aio_task, p);
	get_task_struct(p);

	/* I mean, really?! IO_CMD_NOOP is even defined, but not implemented. */
	int fds[2];
	int ret = pipe(fds);
	if (ret != 0)
		die("pipe err: %s", strerror(ret));

	/* Wake up the completion thread with spurious work. */
	int junk = 0;
	struct iocb iocb = {
		.aio_lio_opcode = IO_CMD_PWRITE,
		.data = NULL, /* Signal to stop */
		.aio_fildes = fds[1],
		.u.c.buf = &junk,
		.u.c.nbytes = 1,
	}, *iocbp = &iocb;
	ret = io_submit(aio_ctx, 1, &iocbp);
	if (ret != 1)
		die("io_submit cleanup err: %s", strerror(-ret));

	ret = kthread_stop(p);
	BUG_ON(ret);

	put_task_struct(p);

	xclose(fds[0]);
	xclose(fds[1]);
}

static void aio_op(struct bio *bio, struct iovec *iov, unsigned i, int opcode)
{
	ssize_t ret;
	struct iocb iocb = {
		.data		= bio,
		.aio_fildes	= bio->bi_bdev->bd_fd,
		.aio_rw_flags	= bio->bi_opf & REQ_FUA ? RWF_SYNC : 0,
		.aio_lio_opcode	= opcode,
		.u.c.buf        = iov,
		.u.c.nbytes     = i,
		.u.c.offset     = bio->bi_iter.bi_sector << 9,

	}, *iocbp = &iocb;

	atomic_inc(&running_requests);

	wait_event(aio_events_completed,
		   (ret = io_submit(aio_ctx, 1, &iocbp)) != -EAGAIN);;

	if (ret != 1)
		die("io_submit err: %s", strerror(-ret));
}

static void aio_read(struct bio *bio, struct iovec *iov, unsigned i)
{
	aio_op(bio, iov, i, IO_CMD_PREADV);
}

static void aio_write(struct bio *bio, struct iovec * iov, unsigned i)
{
	aio_op(bio, iov, i, IO_CMD_PWRITEV);
}

void blkdev_init(void)
{
	fops = fops_list;
	fops->init();
}
