// SPDX-License-Identifier: GPL-2.0

#include "bcachefs.h"

#include "alloc/buckets.h"

#include "btree/iter.h"

#include "data/checksum.h"
#include "data/ec/io.h"
#include "data/ec/trigger.h"
#include "data/extents.h"
#include "data/read.h"

#include "init/error.h"
#include "init/passes.h"
#include "sb/errors.h"

#include <linux/string_choices.h>

#ifdef __KERNEL__

#include <linux/raid/pq.h>
#include <linux/raid/xor.h>

#if LINUX_VERSION_CODE < KERNEL_VERSION(7,1,0)
/*
 * 7.1 replaced xor_blocks() with xor_gen(), which handles MAX_XOR_BLOCKS
 * chunking internally. Shim for older kernels so the caller below stays
 * version-agnostic.
 */
static inline void xor_gen(void *dest, void **srcs,
			   unsigned int src_cnt, unsigned int bytes)
{
	unsigned int i = 0;

	while (i < src_cnt) {
		unsigned int nr = min_t(unsigned int, src_cnt - i, MAX_XOR_BLOCKS);
		xor_blocks(nr, bytes, dest, srcs + i);
		i += nr;
	}
}
#endif

#if LINUX_VERSION_CODE < KERNEL_VERSION(7,2,0)
/*
 * 7.2 renamed the raid6 public interface (raid6: improve the public interface):
 * raid6_call.gen_syndrome() -> raid6_gen_syndrome(), and
 * raid6_{2data,datap}_recov() -> raid6_recov_{2data,datap}(). Map the new names
 * onto the old API on older kernels so the callers below stay version-agnostic.
 */
#define raid6_gen_syndrome(disks, bytes, ptrs)	raid6_call.gen_syndrome(disks, bytes, ptrs)
#define raid6_recov_2data			raid6_2data_recov
#define raid6_recov_datap			raid6_datap_recov
#endif

static void raid5_recov(unsigned disks, unsigned failed_idx,
			size_t size, void **data)
{
	BUG_ON(failed_idx >= disks);

	swap(data[0], data[failed_idx]);
	memcpy(data[0], data[1], size);
	xor_gen(data[0], data + 2, disks - 2, size);
	swap(data[0], data[failed_idx]);
}

static void raid_gen(int nd, int np, size_t size, void **v)
{
	if (np >= 1)
		raid5_recov(nd + np, nd, size, v);
	if (np >= 2)
		raid6_gen_syndrome(nd + np, size, v);
	BUG_ON(np > 2);
}

/*
 * raid6_recov_2data() and raid6_recov_datap() handle at most a page: they stand
 * the zero page in for the missing blocks while generating the syndrome.
 */
static void raid6_recov_paged(int disks, size_t size, int faila, int failb, void **v)
{
	void *p[BCH_BKEY_PTRS_MAX];

	for (size_t offset = 0; offset < size; offset += PAGE_SIZE) {
		size_t bytes = min_t(size_t, size - offset, PAGE_SIZE);

		for (int i = 0; i < disks; i++)
			p[i] = v[i] + offset;

		if (failb < disks - 2)
			raid6_recov_2data(disks, bytes, faila, failb, p);
		else
			raid6_recov_datap(disks, bytes, faila, p);
	}
}

static void raid_rec(int nr, int *ir, int nd, int np, size_t size, void **v)
{
	switch (nr) {
	case 0:
		break;
	case 1:
		if (ir[0] < nd + 1)
			raid5_recov(nd + 1, ir[0], size, v);
		else
			raid6_gen_syndrome(nd + np, size, v);
		break;
	case 2:
		if (ir[1] < nd) {
			/* data+data failure. */
			raid6_recov_paged(nd + np, size, ir[0], ir[1], v);
		} else if (ir[0] < nd) {
			/* data + p/q failure */

			if (ir[1] == nd) /* data + p failure */
				raid6_recov_paged(nd + np, size, ir[0], ir[1], v);
			else { /* data + q failure */
				raid5_recov(nd + 1, ir[0], size, v);
				raid6_gen_syndrome(nd + np, size, v);
			}
		} else {
			raid_gen(nd, np, size, v);
		}
		break;
	default:
		BUG();
	}
}

#else

#include <raid/raid.h>

#endif

/*
 * Free the buffers and give back the memory, without touching buf->io: for
 * callers running as buf->io's own continuation, where the IO is already done
 * (that's why they're running) and destroying the closure underneath
 * themselves would not go well.
 */
void __bch2_ec_stripe_buf_exit(struct ec_stripe_buf *buf)
{
	if (buf->c) {
		struct bch_fs *c = buf->c;
		buf->c = NULL;
		scoped_guard(spinlock, &c->ec.stripe_buf_lock) {
			size_t buf_bytes = ((unsigned long)buf->size << 9) * buf->key.v.nr_blocks;
			c->ec.stripe_buf_bytes -= buf_bytes;
			closure_wake_up(&c->ec.stripe_buf_wait);
		}
	}

	if (buf->key.k.type == KEY_TYPE_stripe) {
		for (unsigned i = 0; i < buf->key.v.nr_blocks; i++) {
			kvfree(buf->data[i]);
			buf->data[i] = NULL;
		}
	}
}

void bch2_ec_stripe_buf_exit(struct ec_stripe_buf *buf)
{
	/*
	 * Drain in-flight stripe IO before freeing the buffers it reads/writes
	 * into: the bios are mapped directly at buf->data[] and hold refs on
	 * buf->io, so freeing first is a use-after-free.
	 */
	closure_sync(&buf->io);

	__bch2_ec_stripe_buf_exit(buf);

	closure_debug_destroy(&buf->io);
}

/*
 * Move @src's buffers, read results and key into @dst, which must be empty. The
 * IO closure belongs to the buffer's owner and doesn't move.
 */
void bch2_ec_stripe_buf_move(struct ec_stripe_buf *dst, struct ec_stripe_buf *src)
{
	EBUG_ON(dst->c);

	closure_sync(&src->io);

	dst->contents = src->contents;

	src->c = NULL;
	memset(src->data, 0, sizeof(src->data));
}

/*
 * Over ec_stripe_buf_limit, waits on @cl if given, fails if
 * EC_STRIPE_BUF_optional, and otherwise goes over it.
 */
int __bch2_ec_stripe_buf_init(struct bch_fs *c,
			      struct ec_stripe_buf *buf,
			      unsigned offset, unsigned size,
			      struct closure *cl, enum ec_stripe_buf_flags flags)
{
	unsigned csum_granularity = 1U << buf->key.v.csum_granularity_bits;
	unsigned end = offset + size;

	BUG_ON(end > le16_to_cpu(buf->key.v.sectors));

	if (!(flags & EC_STRIPE_BUF_unaligned)) {
		offset	= round_down(offset, csum_granularity);
		end	= min_t(unsigned, le16_to_cpu(buf->key.v.sectors),
				round_up(end, csum_granularity));
	}

	unsigned long buf_bytes = ((unsigned long)(end - offset) << 9) *
		buf->key.v.nr_blocks;
	unsigned long limit = (totalram_pages() << PAGE_SHIFT) / 100 *
		c->opts.ec_stripe_buf_limit;

	scoped_guard(spinlock, &c->ec.stripe_buf_lock) {
		if ((cl || (flags & EC_STRIPE_BUF_optional)) &&
		    c->ec.stripe_buf_bytes &&
		    c->ec.stripe_buf_bytes + buf_bytes > limit) {
			if (cl)
				closure_wait(&c->ec.stripe_buf_wait, cl);
			return bch_err_throw(c, stripe_buf_mem_blocked);
		}

		c->ec.stripe_buf_bytes += buf_bytes;
	}

	buf->c		= c;
	buf->offset	= offset;
	buf->size	= end - offset;
	buf->unaligned	= flags & EC_STRIPE_BUF_unaligned;

	for (unsigned i = 0; i < buf->key.v.nr_blocks; i++) {
		buf->data[i] = kvmalloc(buf->size << 9, GFP_KERNEL);
		if (!buf->data[i]) {
			bch2_ec_stripe_buf_exit(buf);
			buf->c = NULL;
			return bch_err_throw(c, ENOMEM_stripe_buf);
		}
	}

	closure_init(&buf->io, NULL);

	return 0;
}

/* Checksumming: */

static struct bch_csum ec_block_checksum(struct ec_stripe_buf *buf,
					 unsigned block, unsigned offset)
{
	unsigned csum_granularity = 1U << buf->key.v.csum_granularity_bits;
	unsigned end = buf->offset + buf->size;
	unsigned len = min(csum_granularity, end - offset);

	BUG_ON(offset >= end);
	BUG_ON(offset <  buf->offset);
	BUG_ON(offset & (csum_granularity - 1));
	BUG_ON(offset + len != le16_to_cpu(buf->key.v.sectors) &&
	       (len & (csum_granularity - 1)));

	return bch2_checksum(NULL, buf->key.v.csum_type,
			     null_nonce(),
			     buf->data[block] + ((offset - buf->offset) << 9),
			     len << 9);
}

void bch2_ec_generate_checksums(struct ec_stripe_buf *buf)
{
	unsigned csums_per_device = stripe_csums_per_device(&buf->key.v);

	if (!buf->key.v.csum_type)
		return;

	BUG_ON(buf->offset);
	BUG_ON(buf->size != le16_to_cpu(buf->key.v.sectors));

	for (unsigned i = 0; i < buf->key.v.nr_blocks; i++)
		for (unsigned j = 0; j < csums_per_device; j++)
			stripe_csum_set(&buf->key.v, i, j,
				ec_block_checksum(buf, i, j << buf->key.v.csum_granularity_bits));
}

static void bch2_ec_validate_checksums(struct bch_fs *c, struct ec_stripe_buf *buf,
				       bool data_only, enum bch_stripe_buf_err e)
{
	unsigned nr_data = buf->key.v.nr_blocks - buf->key.v.nr_redundant;
	unsigned csum_granularity = 1U << buf->key.v.csum_granularity_bits;

	if (!buf->key.v.csum_type || buf->unaligned)
		return;

	for (unsigned i = 0; i < (data_only ? nr_data : buf->key.v.nr_blocks); i++) {
		unsigned offset = buf->offset;
		unsigned end = buf->offset + buf->size;

		if (buf->err[e][i])
			continue;

		while (offset < end) {
			unsigned j = offset >> buf->key.v.csum_granularity_bits;
			unsigned len = min(csum_granularity, end - offset);
			struct bch_csum want = stripe_csum_get(&buf->key.v, i, j);
			struct bch_csum got = ec_block_checksum(buf, i, offset);

			if (bch2_crc_cmp(want, got)) {
				buf->err[e][i] = bch_err_throw(c, stripe_read_csum_err);
				buf->csum_good[i] = want;
				buf->csum_bad[i] = got;

				/*
				 * Can't error on invalid device, we no longer
				 * have the bkey locked
				 */
				CLASS(bch2_dev_tryget_noerror, ca)(c, buf->key.v.ptrs[i].dev);
				if (ca)
					bch2_io_error(ca, BCH_MEMBER_ERROR_checksum);
				break;
			}

			offset += len;
		}
	}
}

/*
 * Scrub: check one block of a stripe against the stripe's checksums for it,
 * reading only that block, a checksum granule at a time. Parity and data no
 * extent references any more are still inputs to reconstruct, and nothing
 * else ever reads them.
 *
 * @buf->key is the stripe; nothing else in @buf is set up. Returns the number
 * of sectors in granules that don't match, or the error if the block couldn't
 * be read - including its pointer going stale, i.e. the stripe was deleted or
 * reused under us.
 */
s64 bch2_ec_scrub_block(struct bch_fs *c, struct ec_stripe_buf *buf, unsigned block)
{
	struct bch_stripe *v = &buf->key.v;
	unsigned granularity = 1U << v->csum_granularity_bits;
	unsigned sectors = le16_to_cpu(v->sectors);
	s64 bad = 0;

	closure_init(&buf->io, NULL);

	if (!v->csum_type)
		return 0;

	buf->data[block] = kvmalloc(min(granularity, sectors) << 9, GFP_KERNEL);
	if (!buf->data[block])
		return bch_err_throw(c, ENOMEM_stripe_buf);

	for (unsigned offset = 0; offset < sectors; offset += granularity) {
		buf->offset	= offset;
		buf->size	= min(granularity, sectors - offset);
		buf->err[STRIPE_BUF_PRE_RECOV][block] = 0;

		bch2_ec_block_io_range(c, buf, REQ_OP_READ, block, buf->offset, buf->size);
		closure_sync(&buf->io);

		int err = buf->err[STRIPE_BUF_PRE_RECOV][block];
		if (err)
			return err;

		struct bch_csum want = stripe_csum_get(v, block, offset >> v->csum_granularity_bits);
		struct bch_csum got = ec_block_checksum(buf, block, offset);

		if (bch2_crc_cmp(want, got)) {
			if (!bad) {
				buf->csum_good[block]	= want;
				buf->csum_bad[block]	= got;
			}
			bad += buf->size;
		}
	}

	if (bad) {
		CLASS(bch2_dev_tryget_noerror, ca)(c, v->ptrs[block].dev);
		if (ca)
			bch2_io_error(ca, BCH_MEMBER_ERROR_checksum);
	}

	return bad;
}

/*
 * Were the blocks in @mask read without error, and do they match the stripe's
 * checksums? Doesn't record anything: a failure here is followed by a full
 * read and bch2_stripe_buf_validate_msg(), which does.
 */
bool bch2_stripe_buf_blocks_good(struct ec_stripe_buf *buf, u32 mask)
{
	unsigned csum_granularity = 1U << buf->key.v.csum_granularity_bits;

	for (unsigned i = 0; i < buf->key.v.nr_blocks; i++) {
		if (!(mask & BIT(i)))
			continue;

		if (buf->err[STRIPE_BUF_PRE_RECOV][i])
			return false;

		if (!buf->key.v.csum_type)
			continue;

		for (unsigned offset = buf->offset;
		     offset < buf->offset + buf->size;
		     offset += csum_granularity) {
			unsigned j = offset >> buf->key.v.csum_granularity_bits;

			if (bch2_crc_cmp(stripe_csum_get(&buf->key.v, i, j),
					 ec_block_checksum(buf, i, offset)))
				return false;
		}
	}

	return true;
}

void bch2_ec_generate_ec(struct ec_stripe_buf *buf)
{
	unsigned nr_data = buf->key.v.nr_blocks - buf->key.v.nr_redundant;
	unsigned bytes = le16_to_cpu(buf->key.v.sectors) << 9;

	raid_gen(nr_data, buf->key.v.nr_redundant, bytes, buf->data);
}

/* Recov */

static int bch2_ec_do_recov(struct bch_fs *c, struct ec_stripe_buf *buf, u32 required)
{
	unsigned failed[BCH_BKEY_PTRS_MAX], nr_failed = 0;
	unsigned nr_data = buf->key.v.nr_blocks - buf->key.v.nr_redundant;
	unsigned bytes = buf->size << 9;

	/*
	 * Nothing the caller wants is bad, so there is nothing to reconstruct.
	 * Damage confined to blocks it isn't going to look at is not an error:
	 * a stripe reuse, for instance, only carries forward blocks holding
	 * live data and regenerates parity from scratch, so a dead block that
	 * holds nothing must not stop it - otherwise the rewrite that would
	 * drop that block can never run.
	 */
	if (!(ec_failed_mask(buf, STRIPE_BUF_PRE_RECOV) & required))
		return 0;

	if (ec_nr_failed(buf, STRIPE_BUF_PRE_RECOV) > buf->key.v.nr_redundant)
		return bch_err_throw(c, stripe_reconstruct_insufficient_blocks);

	/*
	 * The full erasure list, parity included - raid_rec() dispatches on it.
	 * A lost P or Q we don't report doesn't just pick a worse method, it
	 * drops the count: two erasures reported as one takes the single
	 * erasure XOR path, which reconstructs through the parity block we
	 * already know is bad.
	 */
	for (unsigned i = 0; i < buf->key.v.nr_blocks; i++)
		if (buf->err[STRIPE_BUF_PRE_RECOV][i])
			failed[nr_failed++] = i;

	raid_rec(nr_failed, failed, nr_data, buf->key.v.nr_redundant, bytes, buf->data);

	bch2_ec_validate_checksums(c, buf, true, STRIPE_BUF_POST_RECOV);

	return ec_failed_mask(buf, STRIPE_BUF_POST_RECOV) & required
		? bch_err_throw(c, stripe_read_csum_err)
		: 0;
}

/* Validate */

/* A stale read on an unpinned stripe is an expected race, not an error */
static bool stripe_read_maybe_spurious(struct ec_stripe_buf *buf, unsigned i,
				       int err, bool is_open)
{
	return err == -BCH_ERR_stripe_read_ptr_stale &&
		!test_bit(i, buf->stale) &&
		!is_open;
}

/*
 * A device going offline is reported once, by the device. Every stripe that
 * touches it reporting it again is noise: there's nothing to say unless a block
 * failed for some other reason, or we couldn't cope. Blocks a read-around
 * didn't read aren't failures at all.
 */
static bool stripe_errs_only_dev_offline(struct ec_stripe_buf *buf)
{
	for (unsigned e = 0; e < ARRAY_SIZE(buf->err); e++)
		for (unsigned i = 0; i < buf->key.v.nr_blocks; i++)
			if (buf->err[e][i] &&
			    buf->err[e][i] != -BCH_ERR_stripe_read_device_offline &&
			    buf->err[e][i] != -BCH_ERR_stripe_read_skipped)
				return false;
	return true;
}

static __cold void __stripe_buf_errs_to_text(struct printbuf *out, struct bch_fs *c,
				      struct ec_stripe_buf *buf,
				      enum bch_stripe_buf_err e, bool is_open,
				      u32 required)
{
	for (unsigned i = 0; i < buf->key.v.nr_blocks; i++) {
		int err = buf->err[e][i];
		if (err) {
			CLASS(bch2_dev_tryget_noerror, ca)(c, buf->key.v.ptrs[i].dev);
			prt_printf(out, "block %u %s: %s",
				   i,
				   ca ? ca->name : "(invalid device)",
				   bch2_err_str(err));

			if (err == -BCH_ERR_stripe_read_csum_err) {
				prt_str(out, " expected ");
				bch2_csum_to_text(out, buf->key.v.csum_type, buf->csum_good[i]);
				prt_str(out, " got ");
				bch2_csum_to_text(out, buf->key.v.csum_type, buf->csum_bad[i]);
			}

			if (!(BIT(i) & required))
				prt_str(out, " (block not in use)");

			if (e == STRIPE_BUF_PRE_RECOV &&
			    stripe_read_maybe_spurious(buf, i, err, is_open))
				prt_str(out, " (possibly spurious: stripe not pinned)");

			prt_newline(out);
		}
	}
}

static __cold void stripe_buf_errs_to_text(struct printbuf *out, struct bch_fs *c,
				    struct ec_stripe_buf *buf, bool is_open,
				    u32 required)
{
	if (ec_nr_failed(buf, STRIPE_BUF_PRE_RECOV)) {
		prt_printf(out, "Errors pre recovery\n");
		scoped_guard(printbuf_indent, out)
			__stripe_buf_errs_to_text(out, c, buf, STRIPE_BUF_PRE_RECOV,
						  is_open, required);
	}

	if (ec_nr_failed(buf, STRIPE_BUF_POST_RECOV)) {
		prt_printf(out, "Errors post recovery\n");
		scoped_guard(printbuf_indent, out)
			__stripe_buf_errs_to_text(out, c, buf, STRIPE_BUF_POST_RECOV,
						  is_open, required);
	}
}

static int bch2_stripe_buf_validate(struct bch_fs *c, struct ec_stripe_buf *buf,
				    bool is_open, u32 required)
{
	closure_sync(&buf->io);

	bch2_ec_validate_checksums(c, buf, false, STRIPE_BUF_PRE_RECOV);
	if (!ec_nr_failed(buf, STRIPE_BUF_PRE_RECOV))
		return 0;

	bool have_stale_race = false;
	for (unsigned i = 0; i < buf->key.v.nr_blocks; i++) {
		int err = buf->err[STRIPE_BUF_PRE_RECOV][i];

		bool stale_race = stripe_read_maybe_spurious(buf, i, err, is_open);
		have_stale_race |= stale_race;

		/*
		 * A pinned stripe's blocks cannot legitimately go stale under
		 * us - that's the allocator invalidating a bucket a pinned
		 * stripe references, i.e. a filesystem inconsistency: count
		 * it so it's visible in the field and fails tests. Device
		 * offline and IO errors are environmental, not
		 * inconsistencies, and stale reads on unpinned stripes are
		 * an expected race - neither counts.
		 */
		if (is_open && err == -BCH_ERR_stripe_read_ptr_stale)
			bch2_sb_error_count(c, BCH_FSCK_ERR_stripe_read_ptr_stale);
	}
	int ret = bch2_ec_do_recov(c, buf, required);

	if (ret && !is_open && have_stale_race)
		ret = bch_err_throw(c, stripe_reconstruct_stale_race);
	return ret;
}

int bch2_stripe_buf_validate_msg(struct bch_fs *c, struct ec_stripe_buf *buf,
				 bool is_open, u32 required)
{
	int ret = bch2_stripe_buf_validate(c, buf, is_open, required);

	if (!ret &&
	    !ec_nr_failed(buf, STRIPE_BUF_PRE_RECOV) &&
	    !ec_nr_failed(buf, STRIPE_BUF_POST_RECOV))
		return 0;

	if (ret == -BCH_ERR_stripe_reconstruct_stale_race)
		return ret;

	if (!ret && stripe_errs_only_dev_offline(buf))
		return 0;

	/*
	 * Count the device fault and the outcome separately: a checksum error
	 * means a live device handed back data that didn't match, which is how
	 * a failing drive is found, and whether reconstruct then saved us is a
	 * different question. Per block, as the read path counts per failure.
	 *
	 * Device offline and IO errors stay uncounted here for the reason given
	 * at the stale-read case above - they're environmental, and a missing
	 * device is already visible as one.
	 */
	struct bch_stripe *v = &buf->key.v;

	for (unsigned i = 0; i < v->nr_blocks; i++)
		if (buf->err[STRIPE_BUF_PRE_RECOV][i] == -BCH_ERR_stripe_read_csum_err)
			bch2_sb_error_count(c, BCH_FSCK_ERR_stripe_read_csum_err);

	/*
	 * @ret is bch2_ec_do_recov()'s verdict, and it fails two ways: too many
	 * blocks gone to run at all, or running and producing blocks that still
	 * don't check out. Both cost the caller its data.
	 */
	if (ret)
		bch2_sb_error_count(c, BCH_FSCK_ERR_stripe_reconstruct_failed);

	/*
	 * Damage confined to blocks the caller isn't using is not an error: a
	 * stripe reuse discards them. It's still worth saying - it's a device
	 * producing bad blocks - but at notice level, and it's the same stripe
	 * reporting the same dead block every time it comes off the LRU, so it
	 * gets its own ratelimit state (bch2_ratelimit() has one per call site)
	 * rather than eating the budget for damage that did matter.
	 */
	bool damage_matters = ec_failed_mask(buf, STRIPE_BUF_PRE_RECOV) & required;

	CLASS(bch_log_msg_level, msg)(c, ret || damage_matters
				      ? LOGLEVEL_err : LOGLEVEL_notice);

	prt_printf(&msg.m, "%ps(): %s:\n", (void *) _RET_IP_,
		   ret || damage_matters
		   ? "error reading stripe"
		   : "damaged stripe blocks, none in use");
	bch2_bkey_val_to_text(&msg.m, c, bkey_i_to_s_c(&buf->key.k_i));
	prt_newline(&msg.m);

	stripe_buf_errs_to_text(&msg.m, c, buf, is_open, required);

	if (ret) {
		prt_printf(&msg.m, "error: %s\n", bch2_err_str(ret));
		msg.m.suppress = bch2_ratelimit(c);
	} else if (damage_matters) {
		prt_printf(&msg.m, "successful reconstruct\n");
		/* Separate ratelimit state per severity: */
		msg.m.suppress = bch2_ratelimit(c);
	} else {
		msg.m.suppress = bch2_ratelimit(c);
	}

	return ret;
}

/* IO: */

static void ec_block_endio(struct bio *bio)
{
	struct ec_bio *ec_bio = container_of(bio, struct ec_bio, bio);
	struct ec_stripe_buf *buf = ec_bio->buf;
	struct bch_extent_ptr *ptr = &buf->key.v.ptrs[ec_bio->idx];
	struct bch_dev *ca = ec_bio->ca;
	int rw = ec_bio->rw;
	unsigned ref = rw == READ
		? (unsigned) BCH_DEV_READ_REF_ec_block
		: (unsigned) BCH_DEV_WRITE_REF_ec_block;

	bch2_account_io_completion(ca, bio_data_dir(bio),
				   ec_bio->submit_time, !bio->bi_status);
	if (rw == WRITE)
		bch2_dev_write_unflushed(ca);

	if (bio->bi_status)
		buf->err[STRIPE_BUF_PRE_RECOV][ec_bio->idx] = -blk_status_to_bch_err(bio->bi_status);
	else if (dev_ptr_stale(ca, ptr))
		buf->err[STRIPE_BUF_PRE_RECOV][ec_bio->idx] = bch_err_throw(ca->fs, stripe_read_ptr_stale);

	bio_put(&ec_bio->bio);
	enumerated_ref_put(&ca->io_ref[rw], ref);
	closure_put(&buf->io);
}

void bch2_ec_block_io(struct bch_fs *c, struct ec_stripe_buf *buf,
		      blk_opf_t opf, unsigned idx)
{
	bch2_ec_block_io_range(c, buf, opf, idx, buf->offset, buf->size);
}

void bch2_ec_block_io_range(struct bch_fs *c, struct ec_stripe_buf *buf,
			    blk_opf_t opf, unsigned idx,
			    unsigned sector_offset, unsigned sectors)
{
	unsigned offset = 0, bytes = sectors << 9;
	struct bch_extent_ptr *ptr = &buf->key.v.ptrs[idx];
	unsigned nr_data = buf->key.v.nr_blocks - buf->key.v.nr_redundant;
	enum bch_data_type data_type = idx < nr_data
		? BCH_DATA_user
		: BCH_DATA_parity;
	int rw = op_is_write(opf);
	unsigned ref = rw == READ
		? (unsigned) BCH_DEV_READ_REF_ec_block
		: (unsigned) BCH_DEV_WRITE_REF_ec_block;

	struct bch_dev *ca = bch2_dev_get_ioref(c, ptr->dev, rw, ref);
	if (!ca) {
		buf->err[STRIPE_BUF_PRE_RECOV][idx] = bch_err_throw(c, stripe_read_device_offline);
		return;
	}

	int stale = dev_ptr_stale(ca, ptr);
	if (stale) {
		buf->err[STRIPE_BUF_PRE_RECOV][idx] = bch_err_throw(c, stripe_read_ptr_stale);
		enumerated_ref_put(&ca->io_ref[rw], ref);
		return;
	}

	this_cpu_add(ca->io_done->sectors[rw][data_type], sectors);

	while (offset < bytes) {
		unsigned nr_iovecs = min_t(size_t, BIO_MAX_VECS,
					   DIV_ROUND_UP(bytes, PAGE_SIZE));
		unsigned b = min_t(size_t, bytes - offset,
				   nr_iovecs << PAGE_SHIFT);
		struct ec_bio *ec_bio;

		ec_bio = container_of(bio_alloc_bioset(ca->disk_sb.bdev,
						       nr_iovecs,
						       opf,
						       GFP_KERNEL,
						       &c->ec.block_bioset),
				      struct ec_bio, bio);

		ec_bio->ca			= ca;
		ec_bio->buf			= buf;
		ec_bio->idx			= idx;
		ec_bio->rw			= rw;
		ec_bio->submit_time		= local_clock();

		ec_bio->bio.bi_iter.bi_sector	= ptr->offset + sector_offset + (offset >> 9);
		ec_bio->bio.bi_end_io		= ec_block_endio;

		bch2_bio_map(&ec_bio->bio, buf->data[idx] + ((sector_offset - buf->offset) << 9) + offset, b);

		closure_get(&buf->io);
		enumerated_ref_get(&ca->io_ref[rw], ref);

		submit_bio(&ec_bio->bio);

		offset += b;
	}

	enumerated_ref_put(&ca->io_ref[rw], ref);
}

/* recovery read path: */

static int get_stripe_key_trans(struct btree_trans *trans, u64 idx,
				struct ec_stripe_buf *stripe)
{
	CLASS(btree_iter, iter)(trans, BTREE_ID_stripes, POS(0, idx), BTREE_ITER_slots);
	struct bkey_s_c k = bkey_try(bch2_btree_iter_peek_slot(&iter));
	if (k.k->type != KEY_TYPE_stripe)
		return -ENOENT;
	bkey_reassemble(&stripe->key.k_i, k);
	return 0;
}

/* Read-around: */

#define EC_READ_AROUND_MIN_GAIN_NS	(1000ULL * 1000)

static inline u64 ec_dev_read_latency(struct bch_dev *ca)
{
	return atomic64_read(&ca->cur_latency[READ]);
}

/*
 * The latency a read-around has to beat: that of its slowest device, @l, scaled
 * for reading k blocks instead of one, plus the least gain worth having.
 */
static inline u64 ec_read_around_cost(u64 l, unsigned penalty)
{
	return div_u64(l * penalty, 100) + EC_READ_AROUND_MIN_GAIN_NS;
}

/*
 * The blocks a read-around of @block reads: the k fastest of the stripe's other
 * blocks. With two parity blocks there are k + 1 to choose from, and the
 * slowest is left out; on a tie, the highest index, so Q before P before data -
 * rebuilding a data block and Q is an XOR and a syndrome, no Galois field
 * recovery.
 *
 * A device with no read latency sample yet doesn't qualify: what it costs is
 * unknown. Returns 0 if fewer than k blocks qualify; otherwise the mask, and in
 * @l_r the latency of the slowest device in it.
 */
static u32 ec_read_around_blocks(struct bch_fs *c, const struct bch_stripe *v,
				 unsigned block, struct bch_io_failures *failed,
				 u64 *l_r)
{
	unsigned nr_data = v->nr_blocks - v->nr_redundant;
	u64 lat[BCH_BKEY_PTRS_MAX] = {};
	u32 mask = 0;

	scoped_guard(rcu)
		for (unsigned i = 0; i < v->nr_blocks; i++) {
			if (i == block)
				continue;

			struct bch_dev *ca = bch2_dev_rcu_noerror(c, v->ptrs[i].dev);
			if (!ca ||
			    !bch2_dev_is_online(ca) ||
			    dev_ptr_stale_rcu(ca, &v->ptrs[i]) ||
			    (failed && bch2_dev_io_failures(failed, ca->dev_idx)))
				continue;

			lat[i] = ec_dev_read_latency(ca);
			if (lat[i])
				mask |= BIT(i);
		}

	while (hweight32(mask) > nr_data) {
		unsigned slowest = __ffs(mask);

		for (unsigned i = slowest + 1; i < v->nr_blocks; i++)
			if ((mask & BIT(i)) && lat[i] >= lat[slowest])
				slowest = i;
		mask &= ~BIT(slowest);
	}

	if (hweight32(mask) < nr_data)
		return 0;

	*l_r = 0;
	for (unsigned i = 0; i < v->nr_blocks; i++)
		if (mask & BIT(i))
			*l_r = max(*l_r, lat[i]);
	return mask;
}

/*
 * Blocks of @required on devices much slower than the rest, which a read of
 * the whole stripe can rebuild from the others: the read-around rule without
 * the coin toss, for stripe repair and reuse, which read in bulk. At most
 * nr_redundant, less blocks already lost to offline devices, and only when
 * every device read in their place has a latency sample.
 */
u32 bch2_ec_read_around_skip(struct bch_fs *c, const struct bch_stripe *v, u32 required)
{
	unsigned penalty = c->opts.ec_read_around_penalty;
	u64 lat[BCH_BKEY_PTRS_MAX] = {};
	u32 offline = 0, skip = 0;

	if (!penalty)
		return 0;

	scoped_guard(rcu)
		for (unsigned i = 0; i < v->nr_blocks; i++) {
			struct bch_dev *ca = bch2_dev_rcu_noerror(c, v->ptrs[i].dev);

			if (ca && bch2_dev_is_online(ca))
				lat[i] = ec_dev_read_latency(ca);
			else
				offline |= BIT(i);
		}

	for (unsigned budget = v->nr_redundant - min(hweight32(offline), v->nr_redundant);
	     budget;
	     --budget) {
		u32 candidates = required & ~skip & ~offline;
		if (!candidates)
			break;

		unsigned slowest = __ffs(candidates);
		for (unsigned i = slowest + 1; i < v->nr_blocks; i++)
			if ((candidates & BIT(i)) && lat[i] > lat[slowest])
				slowest = i;

		u64 l_r = 0;
		for (unsigned i = 0; i < v->nr_blocks; i++) {
			if ((skip | offline | BIT(slowest)) & BIT(i))
				continue;
			if (!lat[i])
				return skip;
			l_r = max(l_r, lat[i]);
		}

		if (lat[slowest] <= ec_read_around_cost(l_r, penalty))
			break;
		skip |= BIT(slowest);
	}

	return skip;
}

/*
 * Should a read of @pick, which bch2_bkey_pick_read_device() chose to read
 * directly, go around its device instead?
 *
 * A reconstruct reads k blocks instead of one, so only when that is clearly
 * faster. With d the device's read latency and r the cost of the reconstruct
 * (its slowest device's latency, scaled by ec_read_around_penalty, plus 1 ms):
 * never when d <= r - so never on a healthy pool - and otherwise with
 * probability 1 - (r/d)^2. The slow device keeps getting some reads, so its
 * latency estimate recovers when it does.
 *
 * Not for reads that must come from the device (scrub), or retries: a retry
 * after an error has its own reconstruct path, and the retry carrying a
 * read-around decision (BCH_READ_ec_read_around) doesn't roll again.
 *
 * Reconcile reads each device's data on that device's own thread, in LBA
 * order (soft_require_read_device). Those read around it only when it's far
 * slower: by 16x, the factor ptr_better() uses to keep replicated reads there
 * when the other replica is non-rotational.
 *
 * Returns 0 or a transaction restart.
 */
int bch2_ec_read_around_pick(struct btree_trans *trans,
			     struct extent_ptr_decoded *pick,
			     struct bch_io_failures *failed,
			     enum bch_read_flags flags, int preferred_dev)
{
	struct bch_fs *c = trans->c;
	unsigned penalty = c->opts.ec_read_around_penalty;
	bool decided = flags & BCH_READ_ec_read_around;

	if (!penalty ||
	    (flags & BCH_READ_hard_require_read_device) ||
	    ((flags & BCH_READ_in_retry) && !decided))
		return 0;

	if ((flags & BCH_READ_soft_require_read_device) &&
	    pick->ptr.dev == preferred_dev)
		penalty *= 16;

	if (failed && failed->ec_around_errcode)
		return 0;

	/*
	 * Finding the stripe's devices takes a stripes btree lookup, so first
	 * check against every online device: r can't be lower than the fastest.
	 */
	u64 l_d = 0, l_min = U64_MAX;
	scoped_guard(rcu) {
		struct bch_dev *ca = bch2_dev_rcu_noerror(c, pick->ptr.dev);
		if (!ca)
			return 0;

		l_d = ec_dev_read_latency(ca);
		if (l_d <= EC_READ_AROUND_MIN_GAIN_NS)
			return 0;

		for_each_online_member_rcu(c, peer) {
			u64 l = ec_dev_read_latency(peer);
			if (peer != ca && l)
				l_min = min(l_min, l);
		}
	}

	if (l_min == U64_MAX ||
	    l_d <= ec_read_around_cost(l_min, penalty))
		return 0;

	if (!decided) {
		/* Data updates get here unlocked, by bch2_data_update_init(): */
		try(bch2_trans_relock(trans));

		CLASS(btree_iter, iter)(trans, BTREE_ID_stripes, POS(0, pick->ec.idx), BTREE_ITER_slots);
		struct bkey_s_c k = bch2_btree_iter_peek_slot(&iter);
		int ret = bkey_err(k);
		if (ret)
			return bch2_err_matches(ret, BCH_ERR_transaction_restart) ? ret : 0;

		if (k.k->type != KEY_TYPE_stripe)
			return 0;

		const struct bch_stripe *v = bkey_s_c_to_stripe(k).v;
		if (!bch2_ptr_matches_stripe(v, *pick))
			return 0;

		u64 l_r;
		if (!ec_read_around_blocks(c, v, pick->ec.block, failed, &l_r))
			return 0;

		/* In ~us, so the squares fit: */
		u64 d = min_t(u64, l_d >> 10, U32_MAX);
		u64 r = min_t(u64, ec_read_around_cost(l_r, penalty) >> 10, U32_MAX);

		if (d <= r ||
		    bch2_get_random_u64_below(d * d) < r * r)
			return 0;
	}

	pick->do_ec_reconstruct = true;
	pick->ec_read_around	= true;
	return 0;
}

int bch2_ec_read_extent(struct btree_trans *trans, struct bch_read_bio *rbio,
			struct bkey_s_c orig_k,
			struct bch_io_failures *failed)
{
	struct printbuf *msg = &failed->ec_msg;
	/*
	 * We need the original extent to read to still be locked when we check
	 * for non-spurious stale stripe pointers
	 */
	try(bch2_trans_relock(trans));

	struct bch_fs *c = trans->c;

	BUG_ON(!rbio->pick.has_ec);

	struct ec_stripe_buf *buf __free(ec_stripe_buf_free) = kzalloc(sizeof(*buf), GFP_KERNEL);
	if (!buf) {
		prt_printf(msg, "error allocating struct ec_stripe_buf\n");
		return bch_err_throw(c, stripe_reconstruct_enomem);
	}

	int ret = lockrestart_do(trans, get_stripe_key_trans(trans, rbio->pick.ec.idx, buf));
	if (ret) {
		prt_printf(msg, "error looking up stripe\n");
		return bch_err_throw(c, stripe_reconstruct);
	}

	if (!bch2_ptr_matches_stripe(&buf->key.v, rbio->pick)) {
		prt_printf(msg, "pointer doesn't match stripe\n");
		bch2_bkey_val_to_text(msg, c, bkey_i_to_s_c(&buf->key.k_i));
		prt_newline(msg);
		return bch_err_throw(c, stripe_reconstruct);
	}

	unsigned offset = rbio->bio.bi_iter.bi_sector - buf->key.v.ptrs[rbio->pick.ec.block].offset;
	if (offset + bio_sectors(&rbio->bio) > le16_to_cpu(buf->key.v.sectors)) {
		prt_printf(msg, "read is biffer than stripe\n");
		bch2_bkey_val_to_text(msg, c, bkey_i_to_s_c(&buf->key.k_i));
		prt_newline(msg);
		return bch_err_throw(c, stripe_reconstruct);
	}

	/*
	 * Check for stale pointers while we still have btree locks held: the
	 * stripe key was just read under lock, so it's the current live key,
	 * and a live stripe key referencing a stale-gen bucket is an
	 * allocator inconsistency regardless of whether the stripe is pinned
	 * - hence no is_open gating here, unlike the validate-time check.
	 * Here the btree lock is what proves the key is live; post-IO, in
	 * bch2_stripe_buf_validate(), the pin is, because the key may have
	 * been legitimately deleted while the IO was in flight.
	 */
	bool have_stale = false;
	scoped_guard(rcu) {
		for (unsigned i = 0; i < buf->key.v.nr_blocks; i++) {
			struct bch_dev *ca = bch2_dev_rcu_noerror(c, buf->key.v.ptrs[i].dev);
			if (ca && dev_ptr_stale(ca, &buf->key.v.ptrs[i])) {
				__set_bit(i, buf->stale);
				have_stale = true;
			}
		}
	}

	if (have_stale) {
		CLASS(bch_log_msg_ratelimited, msg)(c);
		prt_printf(&msg.m, "Stripe with stale pointer(s):\n");
		bch2_bkey_val_to_text(&msg.m, c, bkey_i_to_s_c(&buf->key.k_i));

		bch2_count_fsck_err(c, stale_dirty_ptr, &msg.m);
		bch2_run_explicit_recovery_pass(c, &msg.m, BCH_RECOVERY_PASS_check_allocations, 0);
	}

	u32 read_mask = EC_BLOCKS_ALL;
	enum ec_stripe_buf_flags buf_flags = 0;
	if (rbio->pick.ec_read_around) {
		u64 l_r;
		read_mask = ec_read_around_blocks(c, &buf->key.v, rbio->pick.ec.block,
						  failed, &l_r);
		/* Raced with a device going offline: not worth a message */
		if (!read_mask)
			return bch_err_throw(c, stripe_reconstruct_insufficient_blocks);

		/*
		 * A read-around is optional, so it doesn't go over the stripe
		 * buffer limit; the read goes to the device instead. When the
		 * extent is checksummed, its checksum checks the result, as
		 * it would a direct read, so read just the extent's range of
		 * each block, without rounding to checksum granules.
		 */
		buf_flags |= EC_STRIPE_BUF_optional;
		if (rbio->pick.crc.csum_type)
			buf_flags |= EC_STRIPE_BUF_unaligned;
	} else if (bch2_dev_io_failures(failed, rbio->pick.ptr.dev)) {
		/*
		 * The block just failed to read, so rebuild it from the
		 * others: on a drive retrying a bad sector, reading it again
		 * costs as long again.
		 */
		read_mask &= ~BIT(rbio->pick.ec.block);
	}

	/* Don't hold btree locks for stripe buffer allocations, or IO */
	bch2_trans_unlock(trans);

	ret = __bch2_ec_stripe_buf_init(c, buf, offset, bio_sectors(&rbio->bio), NULL,
					buf_flags);
	if (bch2_err_matches(ret, BCH_ERR_stripe_buf_mem_blocked))
		return ret;
	if (ret) {
		prt_printf(msg, "error allocating stripe data buffers\n");
		bch2_bkey_val_to_text(msg, c, bkey_i_to_s_c(&buf->key.k_i));
		prt_newline(msg);
		return bch_err_throw(c, stripe_reconstruct_enomem);
	}

	/* Blocks not read are erasures, rebuilt with the target: */
	for (unsigned i = 0; i < buf->key.v.nr_blocks; i++)
		if (read_mask & BIT(i))
			bch2_ec_block_io(c, buf, REQ_OP_READ, i);
		else
			buf->err[STRIPE_BUF_PRE_RECOV][i] = -BCH_ERR_stripe_read_skipped;

	ret = bch2_stripe_buf_validate(c, buf, false, EC_BLOCKS_ALL);
	if (ret == -BCH_ERR_stripe_reconstruct_stale_race)
		return bch_err_throw(c, data_read_ptr_stale_race);

	if (!ret)
		memcpy_to_bio(&rbio->bio, rbio->bio.bi_iter,
			      buf->data[rbio->pick.ec.block] + ((offset - buf->offset) << 9));

	if (!ret && stripe_errs_only_dev_offline(buf))
		return 0;

	stripe_buf_errs_to_text(msg, c, buf, false, EC_BLOCKS_ALL);

	if (!ec_nr_failed(buf, STRIPE_BUF_PRE_RECOV) &&
	    !ec_nr_failed(buf, STRIPE_BUF_POST_RECOV))
		;
	else if (!ret)
		prt_printf(msg, "successful reconstruct\n");
	else
		prt_printf(msg, "error: %s\n", bch2_err_str(ret));

	return ret;
}
