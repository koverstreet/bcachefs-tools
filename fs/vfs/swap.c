// SPDX-License-Identifier: GPL-2.0
#ifndef NO_BCACHEFS_FS

#include "bcachefs.h"
#include "alloc/buckets.h"
#include "btree/cache.h"
#include "btree/iter.h"
#include "btree/update.h"
#include "data/extents.h"
#include "data/io_misc.h"
#include "data/write.h"
#include "vfs/fs.h"
#include "vfs/io.h"
#include "vfs/swap.h"
#include "vfs/direct.h"
#include "vfs/buffered.h"

#include <linux/sched/mm.h>
#include <linux/swap.h>
#include <linux/falloc.h>
#include <linux/ktime.h>

/*
 * Swap file support for bcachefs.
 *
 * Uses the SWP_FS_OPS path (like NFS) so that bcachefs stays in the I/O
 * loop for swap operations.  This enables checksumming, encryption,
 * replication, and multi-device support for swap data.
 *
 * Key design points:
 * - PF_MEMALLOC is set during swap I/O to prevent reclaim re-entry
 * - BCH_WRITE_swap flag propagates the noreclaim context to the
 *   write index worker thread
 */

/*
 * Swap I/O diagnostics.
 *
 * Track in-flight swap ops and detect when they stall.  Under memory
 * pressure the write path can block indefinitely on allocation —
 * we want to crash early with a useful stack trace rather than
 * silently hang.
 */
static atomic_t bch2_swap_inflight = ATOMIC_INIT(0);
static atomic64_t bch2_swap_completed = ATOMIC64_INIT(0);
static atomic64_t bch2_swap_errors = ATOMIC64_INIT(0);

/*
 * Warn after 2 s, BUG (debug builds) after 60 s.  The BUG threshold must
 * clear the legitimate tail under total swap exhaustion: with swap 100%
 * full and the OOM killer active, individual ops measured >5 s while the
 * fs stayed live (millions of completions, zero errors).  A genuine
 * reclaim deadlock parks ops forever, so 60 s still catches it quickly.
 */
#define SWAP_IO_WARN_NS		(2ULL * NSEC_PER_SEC)
#define SWAP_IO_BUG_NS		(60ULL * NSEC_PER_SEC)

int bch2_swap_activate(struct swap_info_struct *sis,
		       struct file *file, sector_t *span)
{
	struct bch_inode_info *inode = file_bch_inode(file);
	struct bch_fs *c = inode->v.i_sb->s_fs_info;

	if (!S_ISREG(inode->v.i_mode))
		return -EINVAL;

	/*
	 * bcachefs is copy-on-write: overwriting a block allocates a new
	 * one, so a swap write can fail with ENOSPC even though the file
	 * already exists at full size. That failure arrives during reclaim,
	 * which is the worst possible time to discover it.
	 *
	 * Reserve the whole file up front. This is exactly what fallocate
	 * does, so use it rather than inventing a second reservation
	 * mechanism - but not via bch2_fallocate_dispatch(), because swapon()
	 * already holds i_rwsem here and the dispatch takes inode_lock.
	 *
	 * The reservation is persistent, so there is nothing to undo in
	 * ->swap_deactivate(): the file keeps its allocation like any other
	 * fallocated file, and swapoff followed by swapon does not have to
	 * find the space again.
	 */
	loff_t size = i_size_read(&inode->v);
	long ret = __bch2_fallocate(inode, FALLOC_FL_KEEP_SIZE, 0, size);
	if (ret) {
		bch_err(c, "swapon: cannot reserve %llu bytes for inode %llu: %s",
			(u64) size, (u64) inode->v.i_ino, bch2_err_str(ret));
		return ret;
	}

	sis->flags |= SWP_FS_OPS;
	*span = sis->pages;

	bch_info(c, "swap activated on inode %llu (%llu pages)",
		 (u64) inode->v.i_ino, (u64)sis->pages);

	return add_swap_extent(sis, 0, sis->max, 0);
}

void bch2_swap_deactivate(struct file *file)
{
	struct bch_inode_info *inode = file_bch_inode(file);
	struct bch_fs *c = inode->v.i_sb->s_fs_info;

	bch_info(c, "swap deactivated on inode %llu", (u64) inode->v.i_ino);
}

/*
 * Swap I/O callback — called for every swap read/write when SWP_FS_OPS
 * is set.  Returns bytes transferred or -EIOCBQUEUED for async I/O.
 */
int bch2_swap_rw(struct kiocb *iocb, struct iov_iter *iter)
{
	struct bch_fs *c = file_inode(iocb->ki_filp)->i_sb->s_fs_info;
	u64 start_ns = ktime_get_ns();
	int rw = iov_iter_rw(iter);

	atomic_inc(&bch2_swap_inflight);

	iocb->ki_flags |= IOCB_DIRECT;

	/*
	 * Prevent reclaim re-entry for both writes AND reads.
	 *
	 * Writes: swap writeback runs during reclaim, so allocations in
	 * the write path must not trigger reclaim (circular dependency).
	 *
	 * Reads: swap-in happens during page fault.  If a read-path
	 * allocation enters reclaim → reclaim tries to swap out other
	 * pages → those writes compete for the same btree locks as the
	 * read → deadlock.
	 *
	 * PF_MEMALLOC bypasses watermarks and skips direct reclaim.
	 */
	unsigned int noreclaim_flags = memalloc_noreclaim_save();

	ssize_t ret;
	if (rw == READ)
		ret = bch2_read_iter(iocb, iter);
	else
		ret = bch2_write_iter(iocb, iter);

	memalloc_noreclaim_restore(noreclaim_flags);

	atomic_dec(&bch2_swap_inflight);

	u64 elapsed_ns = ktime_get_ns() - start_ns;

	if (ret < 0 && ret != -EIOCBQUEUED) {
		atomic64_inc(&bch2_swap_errors);
		bch_err_ratelimited(c, "swap_rw %s error %li at pos %lld "
				    "(inflight=%d completed=%lld errors=%lld)",
				    rw == READ ? "read" : "write",
				    ret, iocb->ki_pos,
				    atomic_read(&bch2_swap_inflight),
				    atomic64_read(&bch2_swap_completed),
				    atomic64_read(&bch2_swap_errors));
	} else {
		atomic64_inc(&bch2_swap_completed);
	}

	/*
	 * Detect stalled swap I/O.  If a single operation takes >2 s,
	 * something is badly wrong (likely PF_MEMALLOC reserves exhausted
	 * or deadlock).  WARN at SWAP_IO_WARN_NS; in debug builds, BUG at
	 * SWAP_IO_BUG_NS to get a full crash dump with symbolized stacks
	 * instead of a silent hang.
	 */
	if (unlikely(elapsed_ns > SWAP_IO_WARN_NS)) {
		bch_err(c, "swap_rw %s STALL: %llu ms at pos %lld "
			"(inflight=%d completed=%lld errors=%lld)",
			rw == READ ? "read" : "write",
			elapsed_ns / NSEC_PER_MSEC, iocb->ki_pos,
			atomic_read(&bch2_swap_inflight),
			atomic64_read(&bch2_swap_completed),
			atomic64_read(&bch2_swap_errors));
		WARN_ON_ONCE(1);
	}
	if (unlikely(elapsed_ns > SWAP_IO_BUG_NS))
		BUG_ON(IS_ENABLED(CONFIG_BCACHEFS_DEBUG));

	return ret;
}

#endif /* NO_BCACHEFS_FS */
