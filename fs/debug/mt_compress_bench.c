// SPDX-License-Identifier: GPL-2.0
#ifdef CONFIG_BCACHEFS_TESTS

#include "bcachefs.h"

#include "data/compress.h"
#include "data/compress_workers.h"

#include "closure.h"

#include "tests.h"

#include <linux/printk.h>
#include <linux/slab.h>
#include <linux/string.h>

/*
 * MT compression micro-benchmark.
 *
 * Compresses N independent buffers (each sized to encoded_extent_max) two
 * ways, back-to-back, on the same source data:
 *
 *   - Serial:   one chunk at a time on the calling thread, calling
 *               bch2_compress_locked directly.  The workers' workspaces are
 *               still consulted so the serial path exercises the same
 *               per-codec state.
 *
 *   - Parallel: N works submitted via bch2_compress_wq_submit, blocked on a
 *               parent closure.  On a system with >1 compress worker the
 *               codec work should be distributed across multiple threads and
 *               wall time should approach (serial / nr_workers).
 *
 * Each run is repeated BENCH_NR_ITERS times and the wall time is summed -
 * the per-chunk time on a 256K chunk is in the tens-of-microseconds range
 * for lz4, so a single iteration is noisy.
 *
 * Results are emitted to dmesg with a stable "MT_COMPRESS_BENCH:" prefix
 * that a userspace wrapper greps for.  See tests/mt_compress_bench.sh.
 *
 * Gating: the MT workqueue is only initialized when bch2_fs_compress_init
 * runs successfully, and only takes effect when bch2_compress_nr_workers() >
 * 1 (see fs/data/write.c bch2_write_should_mt_compress).  This bench
 * surfaces both numbers in its header line so the operator can tell which
 * case the run is exercising.
 */

#define BENCH_CHUNK_BYTES	(256 * 1024)	/* one encoded_extent_max */
#define BENCH_NR_CHUNKS_MAX	32
#define BENCH_NR_CHUNKS		8
#define BENCH_NR_ITERS		4
#define BENCH_SRC_PATTERN	"bcachefs-mt-compress-bench-pattern-1234567890"

static void bench_fill_src(void *p, size_t len)
{
	/* A repeating text pattern is highly compressible and exercises the
	 * codec's actual transform; a pure-zero buffer hits a fast path in
	 * some compressors that doesn't parallelize the same way. */
	const char *pat = BENCH_SRC_PATTERN;
	size_t plen = strlen(pat);
	u8 *b = p;

	for (size_t i = 0; i < len; i++)
		b[i] = pat[i % plen];
}

/*
 * Run nr compressions serially on the calling thread.  Borrows a worker
 * workspace / verify_buf (if available) so the serial path uses the same
 * per-codec state as the parallel path - otherwise serial looks artificially
 * cheap because it skips mempool_alloc.
 */
static int bench_one_serial(struct bch_fs *c, unsigned compression_opt,
			    void *const *srcs, void *const *dsts,
			    size_t *dst_lens, unsigned nr, size_t chunk_bytes)
{
	struct bch_compress_wq *cwq = c->compress.mt_wq;
	int ret = 0;

	for (unsigned i = 0; i < nr; i++) {
		void *workspace = NULL, *verify_buf = NULL;
		size_t dst_len = chunk_bytes, src_len = chunk_bytes;

		if (cwq) {
			struct bch_compress_worker *w =
				&cwq->workers[i % cwq->nr_workers];
			workspace = w->workspace;
			verify_buf = w->verify_buf;
		}

		unsigned type = bch2_compress_locked(c,
					dsts[i], &dst_len,
					(void *) srcs[i], &src_len,
					compression_opt, POS(0, 0),
					workspace, verify_buf);
		dst_lens[i] = dst_len;

		if (type == BCH_COMPRESSION_TYPE_incompressible) {
			pr_warn("MT_COMPRESS_BENCH: serial chunk %u marked incompressible; check codec support\n",
				i);
			ret = -EINVAL;
			break;
		}
	}
	return ret;
}

/*
 * Run nr compressions in parallel via the MT workqueue.  Caller blocks on
 * the parent closure until all works have run their endio (closure_put).
 */
static int bench_one_parallel(struct bch_fs *c, unsigned compression_opt,
			      void *const *srcs, void *const *dsts,
			      size_t *dst_lens, unsigned nr, size_t chunk_bytes)
{
	struct bch_compress_wq *cwq = c->compress.mt_wq;
	struct bch_compress_work *works;
	struct closure parent;
	int ret = 0;

	works = kcalloc(nr, sizeof(*works), GFP_KERNEL);
	if (!works)
		return -ENOMEM;

	closure_init_stack(&parent);

	for (unsigned i = 0; i < nr; i++) {
		struct bch_compress_worker *w =
			&cwq->workers[i % cwq->nr_workers];
		bch2_compress_wq_submit(&works[i], cwq, &parent,
					compression_opt, POS(0, 0),
					srcs[i], chunk_bytes,
					dsts[i], chunk_bytes,
					w);
	}

	closure_sync(&parent);

	for (unsigned i = 0; i < nr; i++) {
		dst_lens[i] = works[i].dst_len;
		if (works[i].compression_type ==
		    BCH_COMPRESSION_TYPE_incompressible) {
			pr_warn("MT_COMPRESS_BENCH: parallel chunk %u marked incompressible; check codec support\n",
				i);
			ret = -EINVAL;
			break;
		}
	}

	kfree(works);
	return ret;
}

/*
 * Per-codec workhorse.  Returns 0 on success, negative errno on failure.
 *
 * Output line schema (one line per result, prefix-stable for shell-grepping):
 *
 *   MT_COMPRESS_BENCH: opt=<lz4|zstd:N|gzip:N> nr_workers=<n> chunks=<c> chunk_bytes=<b> iters=<i>
 *   MT_COMPRESS_BENCH: serial_ns=<n>
 *   MT_COMPRESS_BENCH: parallel_ns=<n>
 *   MT_COMPRESS_BENCH: speedup=<serial/parallel>x
 *   MT_COMPRESS_BENCH: <PASS|FAIL: reason>
 *
 * 'PASS' requires parallel_ns < serial_ns / 2 on a system with >1 worker.
 * With <=1 worker, the parallel path serializes and we print a SKIP line
 * instead of FAILing.
 */
int bch2_compress_bench(struct bch_fs *c, unsigned compression_opt)
{
	struct bch_compress_wq *cwq = c->compress.mt_wq;
	unsigned nr_workers = cwq ? cwq->nr_workers
				  : bch2_compress_nr_workers();
	unsigned chunk_bytes = min_t(unsigned, BENCH_CHUNK_BYTES,
				      c->opts.encoded_extent_max);
	const unsigned nr = min_t(unsigned, BENCH_NR_CHUNKS,
				  min(nr_workers * 2, BENCH_NR_CHUNKS_MAX));
	void *srcs[BENCH_NR_CHUNKS_MAX] = {};
	void *dsts[BENCH_NR_CHUNKS_MAX] = {};
	size_t dst_lens[BENCH_NR_CHUNKS_MAX];
	u64 serial_ns = 0, parallel_ns = 0;
	int ret = 0;

	if (!chunk_bytes) {
		pr_err("MT_COMPRESS_BENCH: encoded_extent_max is 0; cannot run\n");
		return -EINVAL;
	}

	/*
	 * The MT path is optional - on init failure bch2_fs_compress_init
	 * logs a notice and leaves mt_wq NULL so writes fall back to
	 * serial.  In that state the bench has nothing meaningful to
	 * measure, so fail with a clear message instead of crashing on
	 * a NULL deref in bench_one_parallel.
	 */
	if (!cwq) {
		pr_err("MT_COMPRESS_BENCH: mt_wq is not initialized; cannot run\n");
		return -ENODEV;
	}

	pr_info("MT_COMPRESS_BENCH: opt=0x%x nr_workers=%u chunks=%u chunk_bytes=%u iters=%u\n",
		compression_opt, nr_workers, nr, chunk_bytes, BENCH_NR_ITERS);

	for (unsigned i = 0; i < nr; i++) {
		srcs[i] = kvmalloc(chunk_bytes, GFP_KERNEL);
		dsts[i] = kvmalloc(chunk_bytes, GFP_KERNEL);
		if (!srcs[i] || !dsts[i]) {
			ret = -ENOMEM;
			goto out;
		}
		bench_fill_src(srcs[i], chunk_bytes);
	}

	/* Warm-up: one serial + one parallel run, not timed.  Faults in the
	 * workspaces and any cold page-cache state so the timed runs are
	 * representative. */
	ret = bench_one_serial(c, compression_opt,
			       (void *const *) srcs, dsts, dst_lens,
			       nr, chunk_bytes);
	if (ret)
		goto out;

	ret = bench_one_parallel(c, compression_opt,
				 (void *const *) srcs, dsts, dst_lens,
				 nr, chunk_bytes);
	if (ret)
		goto out;

	for (unsigned it = 0; it < BENCH_NR_ITERS; it++) {
		u64 t0, t1;

		t0 = ktime_get_ns();
		ret = bench_one_serial(c, compression_opt,
				       (void *const *) srcs, dsts, dst_lens,
				       nr, chunk_bytes);
		t1 = ktime_get_ns();
		if (ret)
			goto out;
		serial_ns += t1 - t0;

		t0 = ktime_get_ns();
		ret = bench_one_parallel(c, compression_opt,
					 (void *const *) srcs, dsts, dst_lens,
					 nr, chunk_bytes);
		t1 = ktime_get_ns();
		if (ret)
			goto out;
		parallel_ns += t1 - t0;
	}

	pr_info("MT_COMPRESS_BENCH: serial_ns=%llu\n", serial_ns);
	pr_info("MT_COMPRESS_BENCH: parallel_ns=%llu\n", parallel_ns);
	if (parallel_ns) {
		unsigned int speedup_100 = (unsigned int)(serial_ns * 100 / parallel_ns);
		pr_info("MT_COMPRESS_BENCH: speedup=%u.%02ux\n",
			speedup_100 / 100, speedup_100 % 100);
	} else {
		pr_info("MT_COMPRESS_BENCH: speedup=N/A (parallel_ns=0)\n");
	}

	if (nr_workers < 2) {
		pr_notice("MT_COMPRESS_BENCH: SKIP - mt_wq has %u worker(s); parallel path is effectively serial\n",
			  nr_workers);
		ret = 0;
		goto out;
	}

	if (parallel_ns * 2 < serial_ns) {
		pr_info("MT_COMPRESS_BENCH: PASS - parallel >= 2x faster than serial\n");
		ret = 0;
	} else {
		pr_err("MT_COMPRESS_BENCH: FAIL - parallel (%llu ns) is not >= 2x faster than serial (%llu ns)\n",
		       parallel_ns, serial_ns);
		ret = -EIO;
	}

out:
	for (unsigned i = 0; i < nr; i++) {
		kvfree(srcs[i]);
		kvfree(dsts[i]);
	}
	return ret;
}

#endif /* CONFIG_BCACHEFS_TESTS */
