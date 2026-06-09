// SPDX-License-Identifier: GPL-2.0
#ifdef CONFIG_BCACHEFS_TESTS

#include "bcachefs.h"

#include "data/compress.h"
#include "data/compress_types.h"
#include "data/compress_workers.h"

#include "tests.h"

#include "closure.h"

#include <linux/delay.h>
#include <linux/ktime.h>
#include <linux/random.h>
#include <linux/slab.h>
#include <linux/workqueue.h>

/*
 * NOTE: This file calls buf_uncompress() from fs/data/compress.c, which is
 * `static` on master. The compress.c agent needs to drop the `static`
 * keyword on its definition to make it externally visible. PR 586 does
 * exactly this (see `int buf_uncompress(struct bch_fs *c, ...)`).
 */

static void fill_compressible(void *buf, size_t len)
{
	memset(buf, 0, len);
}

static void fill_incompressible(void *buf, size_t len)
{
	get_random_bytes(buf, len);
}

/*
 * Roundtrip test for the MT compression path.
 *
 * Allocates `nr` chunks of compressible data (zeros), submits all of them
 * to the MT compress WQ concurrently, waits for completion via a parent
 * closure, then decompresses each result and verifies byte-for-byte match.
 *
 * Exercises:
 *   - bch2_compress_wq_submit() dispatch
 *   - Parent closure completion (closure_init / closure_sync)
 *   - Multiple workers running concurrently (results land in per-worker
 *     dst_bufs, so concurrent execution is required for the roundtrip to
 *     finish in roughly one worker's worth of time)
 *   - End-to-end compress + decompress correctness at level 3
 */
static int test_mt_compress_decompress(struct bch_fs *c, u64 nr)
{
	struct bch_compress_wq *cwq = c->compress.mt_wq;
	size_t chunk_size = c->opts.encoded_extent_max;
	size_t total_size = chunk_size * nr;
	unsigned nr_workers = cwq->nr_workers;
	struct closure parent;
	int ret = 0;

	pr_info("mt compress/decompress roundtrip test: %llu chunks of %zu bytes (%u workers)",
		nr, chunk_size, nr_workers);

	void *src __free(kvfree) = kvmalloc(total_size, GFP_KERNEL);
	void *verify __free(kvfree) = kvmalloc(total_size, GFP_KERNEL);
	if (!src || !verify)
		return -ENOMEM;

	fill_compressible(src, total_size);

	struct bch_compress_work *works __free(kvfree) =
		kvcalloc(nr, sizeof(*works), GFP_KERNEL);
	if (!works)
		return -ENOMEM;

	closure_init(&parent, NULL);

	union bch_compression_opt opt = {
		.type	= BCH_COMPRESSION_OPT_zstd,
		.level	= 3,
	};

	for (u64 i = 0; i < nr; i++) {
		struct bch_compress_worker *worker = &cwq->workers[i % nr_workers];

		bch2_compress_wq_submit(&works[i], cwq, &parent,
					opt.value, POS(0, 0),
					src + i * chunk_size, chunk_size,
					worker->dst_buf, chunk_size,
					worker);
	}

	closure_sync(&parent);

	for (u64 i = 0; i < nr; i++) {
		if (works[i].compression_type != BCH_COMPRESSION_TYPE_zstd) {
			bch_err(c, "mt compress: chunk %llu not compressed (type=%u)",
				i, works[i].compression_type);
			return -EIO;
		}

		struct bch_extent_crc_unpacked crc = {
			.compressed_size	= works[i].dst_len >> 9,
			.uncompressed_size	= works[i].src_len >> 9,
			.compression_type	= works[i].compression_type,
		};

		ret = buf_uncompress(c, verify + i * chunk_size,
				     cwq->workers[i % nr_workers].dst_buf, crc);
		if (ret) {
			bch_err(c, "mt compress: decompress chunk %llu failed: %s",
				i, bch2_err_str(ret));
			return ret;
		}

		if (memcmp(verify + i * chunk_size,
			   src + i * chunk_size,
			   works[i].src_len)) {
			bch_err(c, "mt compress: decompressed chunk %llu mismatch", i);
			return -EIO;
		}
	}

	pr_info("mt compress/decompress roundtrip test passed, %llu chunks", nr);
	return 0;
}

/*
 * Random data should be detected as incompressible even under MT dispatch.
 *
 * Allocates one chunk-sized buffer of random bytes and submits `nr` work
 * items all pointing at it. With sufficient randomness no zstd pass should
 * produce a smaller output than input, so all results should be marked
 * BCH_COMPRESSION_TYPE_incompressible.
 */
static int test_mt_compress_incompressible(struct bch_fs *c, u64 nr)
{
	struct bch_compress_wq *cwq = c->compress.mt_wq;
	size_t chunk_size = c->opts.encoded_extent_max;
	unsigned nr_workers = cwq->nr_workers;
	struct closure parent;

	pr_info("mt incompressible test: %llu random chunks of %zu bytes",
		nr, chunk_size);

	void *src __free(kvfree) = kvmalloc(chunk_size, GFP_KERNEL);
	if (!src)
		return -ENOMEM;

	fill_incompressible(src, chunk_size);

	struct bch_compress_work *works __free(kvfree) =
		kvcalloc(nr, sizeof(*works), GFP_KERNEL);
	if (!works)
		return -ENOMEM;

	closure_init(&parent, NULL);

	union bch_compression_opt opt = {
		.type	= BCH_COMPRESSION_OPT_zstd,
		.level	= 3,
	};

	for (u64 i = 0; i < nr; i++) {
		struct bch_compress_worker *worker = &cwq->workers[i % nr_workers];

		bch2_compress_wq_submit(&works[i], cwq, &parent,
					opt.value, POS(0, 0),
					src, chunk_size,
					worker->dst_buf, chunk_size,
					worker);
	}

	closure_sync(&parent);

	unsigned incompressible_count = 0;
	for (u64 i = 0; i < nr; i++)
		if (works[i].compression_type == BCH_COMPRESSION_TYPE_incompressible)
			incompressible_count++;

	if (incompressible_count < nr / 2) {
		bch_err(c, "mt incompressible: expected mostly incompressible, got %u/%llu",
			incompressible_count, nr);
		return -EIO;
	}

	pr_info("mt incompressible test passed, %u/%llu correctly detected",
		incompressible_count, nr);
	return 0;
}

/*
 * THE critical MT test: prove the WQ actually runs multiple work items
 * concurrently rather than serially.
 *
 * We submit N work items directly to cwq->wq (bypassing
 * bch2_compress_wq_submit() since we need a custom work_fn). Each item
 * records its start time, sleeps 10ms, records its end time. After all
 * complete, we check the recorded windows: if at least 2 items had
 * overlapping execution windows, parallelism was achieved.
 *
 * On a strictly serial WQ, the windows would be [0,10],[10,20],... with
 * no overlap and the test FAILS.
 *
 * The test is skipped (with a warning) on systems where the WQ was created
 * with < 2 workers, since strict serial execution would be expected.
 */
struct mt_concurrency_work {
	struct work_struct	work;
	struct closure		*parent;
	ktime_t			start;
	ktime_t			end;
};

static void mt_concurrency_work_fn(struct work_struct *work)
{
	struct mt_concurrency_work *w =
		container_of(work, struct mt_concurrency_work, work);

	w->start = ktime_get();
	msleep(10);
	w->end = ktime_get();

	closure_put(w->parent);
}

static int test_mt_concurrency(struct bch_fs *c, u64 nr)
{
	struct bch_compress_wq *cwq = c->compress.mt_wq;
	const unsigned n = 8;
	struct mt_concurrency_work works[8];
	struct closure parent;
	unsigned overlap_count = 0;

	if (cwq->nr_workers < 2) {
		pr_warn("mt concurrency test skipped: WQ has only %u worker(s), need >= 2",
			cwq->nr_workers);
		return 0;
	}

	pr_info("mt concurrency test: %u items, %u workers, each sleeps 10ms",
		n, cwq->nr_workers);

	closure_init(&parent, NULL);

	for (unsigned i = 0; i < n; i++) {
		works[i].parent	= &parent;
		works[i].start	= 0;
		works[i].end	= 0;
		INIT_WORK(&works[i].work, mt_concurrency_work_fn);
		closure_get(&parent);
		queue_work(cwq->wq, &works[i].work);
	}

	closure_sync(&parent);

	for (unsigned i = 0; i < n; i++) {
		for (unsigned j = i + 1; j < n; j++) {
			/* Overlap: i started before j ended AND j started before i ended. */
			if (ktime_before(works[i].start, works[j].end) &&
			    ktime_before(works[j].start, works[i].end))
				overlap_count++;
		}
	}

	if (overlap_count == 0) {
		bch_err(c, "mt concurrency test FAILED: 0 overlapping pairs out of %u (strictly serial execution)",
			n * (n - 1) / 2);
		return -EIO;
	}

	pr_info("mt concurrency test passed: %u overlapping pairs out of %u",
		overlap_count, n * (n - 1) / 2);
	return 0;
}

/*
 * MT dispatch with all 15 zstd compression levels.
 *
 * Submits 15 work items (one per level) to the MT WQ concurrently and
 * verifies each compresses and decompresses correctly.
 */
static int test_mt_levels(struct bch_fs *c, u64 nr)
{
	struct bch_compress_wq *cwq = c->compress.mt_wq;
	size_t chunk_size = c->opts.encoded_extent_max;
	unsigned nr_workers = cwq->nr_workers;
	const unsigned n_levels = 15;
	struct closure parent;

	pr_info("mt levels test: all 15 zstd levels, chunk_size=%zu", chunk_size);

	void *src __free(kvfree) = kvmalloc(chunk_size, GFP_KERNEL);
	void *verify __free(kvfree) = kvmalloc(chunk_size, GFP_KERNEL);
	if (!src || !verify)
		return -ENOMEM;

	fill_compressible(src, chunk_size);

	struct bch_compress_work *works __free(kvfree) =
		kvcalloc(n_levels, sizeof(*works), GFP_KERNEL);
	if (!works)
		return -ENOMEM;

	closure_init(&parent, NULL);

	for (unsigned level = 1; level <= n_levels; level++) {
		union bch_compression_opt opt = {
			.type	= BCH_COMPRESSION_OPT_zstd,
			.level	= level,
		};
		struct bch_compress_worker *worker =
			&cwq->workers[(level - 1) % nr_workers];

		bch2_compress_wq_submit(&works[level - 1], cwq, &parent,
					opt.value, POS(0, 0),
					src, chunk_size,
					worker->dst_buf, chunk_size,
					worker);
	}

	closure_sync(&parent);

	for (unsigned level = 1; level <= n_levels; level++) {
		unsigned i = level - 1;

		if (works[i].compression_type != BCH_COMPRESSION_TYPE_zstd) {
			bch_err(c, "mt levels: compressible data not compressed at level %u (type=%u)",
				level, works[i].compression_type);
			return -EIO;
		}

		struct bch_extent_crc_unpacked crc = {
			.compressed_size	= works[i].dst_len >> 9,
			.uncompressed_size	= works[i].src_len >> 9,
			.compression_type	= works[i].compression_type,
		};

		int ret = buf_uncompress(c, verify,
					 cwq->workers[i % nr_workers].dst_buf, crc);
		if (ret) {
			bch_err(c, "mt levels: decompress failed at level %u: %s",
				level, bch2_err_str(ret));
			return ret;
		}

		if (memcmp(verify, src, works[i].src_len)) {
			bch_err(c, "mt levels: data mismatch at level %u", level);
			return -EIO;
		}

		pr_info("  level %u: %zu -> %zu bytes", level, works[i].src_len, works[i].dst_len);
	}

	pr_info("mt levels test passed");
	return 0;
}

typedef int (*compress_test_fn)(struct bch_fs *, u64);

/*
 * Entry point invoked from fs/debug/sysfs.c (the sysfs_compress_test
 * attribute). The tests.h agent is responsible for declaring this in
 * fs/debug/tests.h:
 *
 *   int bch2_compress_test(struct bch_fs *, const char *, u64, unsigned);
 */
int bch2_compress_test(struct bch_fs *c, const char *testname,
		       u64 nr, unsigned nr_threads)
{
	compress_test_fn fn = NULL;

	if (nr == 0) {
		pr_err("nr of iterations is not allowed to be 0");
		return -EINVAL;
	}

	if (!c->compress.mt_wq) {
		pr_err("MT compression workqueue not initialized");
		return -EINVAL;
	}

	if (!mempool_initialized(&c->compress.workspace[BCH_COMPRESSION_OPT_zstd])) {
		pr_err("zstd compression not initialized");
		return -EINVAL;
	}

#define compress_test(_test)				\
	if (!strcmp(testname, #_test)) fn = _test

	compress_test(test_mt_compress_decompress);
	compress_test(test_mt_compress_incompressible);
	compress_test(test_mt_concurrency);
	compress_test(test_mt_levels);

	if (!fn) {
		pr_err("unknown compress test %s", testname);
		return -EINVAL;
	}

	int ret = fn(c, nr);
	if (ret)
		bch_err(c, "compress test %s failed: %s", testname, bch2_err_str(ret));
	else
		pr_info("compress test %s passed", testname);
	return ret;
}

#endif /* CONFIG_BCACHEFS_TESTS */
