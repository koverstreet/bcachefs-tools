// SPDX-License-Identifier: GPL-2.0
#ifdef CONFIG_BCACHEFS_TESTS

#include "bcachefs.h"

#include "data/compress.h"
#include "data/compress_types.h"

#include "tests.h"

#include "linux/random.h"
#include "linux/zstd.h"

static void fill_compressible(void *buf, size_t len)
{
	memset(buf, 0, len);
}

static void fill_incompressible(void *buf, size_t len)
{
	get_random_bytes(buf, len);
}

static int test_zstd_compress_decompress(struct bch_fs *c, u64 nr)
{
	size_t src_len = min(c->opts.encoded_extent_max, 256ULL << 10);
	size_t dst_len = src_len;
	int ret = 0;

	pr_info("zstd compress/decompress roundtrip test, src_len=%zu", src_len);

	void *src __cleanup(kfree) = kmalloc(src_len, GFP_KERNEL);
	void *dst __cleanup(kfree) = kmalloc(dst_len, GFP_KERNEL);
	void *verify __cleanup(kfree) = kmalloc(src_len, GFP_KERNEL);
	if (!src || !dst || !verify)
		return -ENOMEM;

	for (u64 i = 0; i < nr; i++) {
		size_t this_src = src_len;
		size_t this_dst = dst_len;

		if (i & 1)
			fill_compressible(src, this_src);
		else
			fill_incompressible(src, this_src);

		union bch_compression_opt compression = {
			.type	= BCH_COMPRESSION_OPT_zstd,
			.level	= (i % 15) + 1,
		};

		unsigned type = bch2_compress(c, dst, &this_dst,
					      src, &this_src,
					      compression.value,
					      POS(0, 0));

		if (type == BCH_COMPRESSION_TYPE_incompressible) {
			if (i & 1) {
				bch_err(c, "test_zstd: compressible data marked incompressible (iter %llu, level %u)",
					i, compression.level);
				return -EIO;
			}
			continue;
		}

		if (type != BCH_COMPRESSION_TYPE_zstd) {
			bch_err(c, "test_zstd: unexpected compression type %u (iter %llu)",
				type, i);
			return -EIO;
		}

		struct bch_extent_crc_unpacked crc = {
			.compressed_size	= this_dst >> 9,
			.uncompressed_size	= this_src >> 9,
			.compression_type	= type,
		};

		ret = buf_uncompress(c, verify, dst, crc);
		if (ret) {
			bch_err(c, "test_zstd: decompression failed (iter %llu, level %u): %s",
				i, compression.level, bch2_err_str(ret));
			return ret;
		}

		if (memcmp(verify, src, this_src)) {
			bch_err(c, "test_zstd: decompressed data mismatch (iter %llu, level %u)",
				i, compression.level);
			return -EIO;
		}
	}

	pr_info("zstd compress/decompress roundtrip test passed, %llu iterations", nr);
	return 0;
}

static int test_zstd_early_abort_incompressible(struct bch_fs *c, u64 nr)
{
	size_t src_len = min(c->opts.encoded_extent_max, 256ULL << 10);
	size_t dst_len = src_len;

	pr_info("zstd early abort test with random data, src_len=%zu", src_len);

	void *src __cleanup(kfree) = kmalloc(src_len, GFP_KERNEL);
	void *dst __cleanup(kfree) = kmalloc(dst_len, GFP_KERNEL);
	if (!src || !dst)
		return -ENOMEM;

	unsigned incompressible_count = 0;

	for (u64 i = 0; i < nr; i++) {
		size_t this_src = src_len;
		size_t this_dst = dst_len;

		fill_incompressible(src, this_src);

		union bch_compression_opt compression = {
			.type	= BCH_COMPRESSION_OPT_zstd,
			.level	= 3,
		};

		unsigned type = bch2_compress(c, dst, &this_dst,
					      src, &this_src,
					      compression.value,
					      POS(0, 0));

		if (type == BCH_COMPRESSION_TYPE_incompressible)
			incompressible_count++;
	}

	if (incompressible_count < nr / 2) {
		bch_err(c, "test_zstd_early_abort: expected mostly incompressible, got %u/%llu",
			incompressible_count, nr);
		return -EIO;
	}

	pr_info("zstd early abort test passed, %u/%llu correctly detected as incompressible",
		incompressible_count, nr);
	return 0;
}

static int test_zstd_levels(struct bch_fs *c, u64 nr)
{
	size_t src_len = min(c->opts.encoded_extent_max, 128ULL << 10);
	size_t dst_len = src_len;

	pr_info("zstd levels test, testing all 15 levels");

	void *src __cleanup(kfree) = kmalloc(src_len, GFP_KERNEL);
	void *dst __cleanup(kfree) = kmalloc(dst_len, GFP_KERNEL);
	void *verify __cleanup(kfree) = kmalloc(src_len, GFP_KERNEL);
	if (!src || !dst || !verify)
		return -ENOMEM;

	fill_compressible(src, src_len);

	for (unsigned level = 1; level <= 15; level++) {
		size_t this_src = src_len;
		size_t this_dst = dst_len;

		union bch_compression_opt compression = {
			.type	= BCH_COMPRESSION_OPT_zstd,
			.level	= level,
		};

		unsigned type = bch2_compress(c, dst, &this_dst,
					      src, &this_src,
					      compression.value,
					      POS(0, 0));

		if (type != BCH_COMPRESSION_TYPE_zstd) {
			bch_err(c, "test_zstd_levels: compressible data not compressed at level %u (type=%u)",
				level, type);
			return -EIO;
		}

		struct bch_extent_crc_unpacked crc = {
			.compressed_size	= this_dst >> 9,
			.uncompressed_size	= this_src >> 9,
			.compression_type	= type,
		};

		int ret = buf_uncompress(c, verify, dst, crc);
		if (ret) {
			bch_err(c, "test_zstd_levels: decompression failed at level %u: %s",
				level, bch2_err_str(ret));
			return ret;
		}

		if (memcmp(verify, src, this_src)) {
			bch_err(c, "test_zstd_levels: data mismatch at level %u", level);
			return -EIO;
		}

		pr_info("  level %u: %zu -> %zu bytes", level, this_src, this_dst);
	}

	pr_info("zstd levels test passed");
	return 0;
}

typedef int (*perf_test_fn)(struct bch_fs *, u64);

struct compress_test_job {
	struct bch_fs		*c;
	u64			nr;
	perf_test_fn		fn;
};

static int compress_test_thread(void *data)
{
	struct compress_test_job *j = data;
	return j->fn(j->c, j->nr);
}

int bch2_compress_test(struct bch_fs *c, const char *testname,
		       u64 nr, unsigned nr_threads)
{
	struct compress_test_job j = { .c = c, .nr = nr };

	if (nr == 0) {
		pr_err("nr of iterations is not allowed to be 0");
		return -EINVAL;
	}

	if (!mempool_initialized(&c->compress.workspace[BCH_COMPRESSION_OPT_zstd])) {
		pr_err("zstd compression not initialized");
		return -EINVAL;
	}

#define compress_test(_test)			\
	if (!strcmp(testname, #_test)) j.fn = _test

	compress_test(test_zstd_compress_decompress);
	compress_test(test_zstd_early_abort_incompressible);
	compress_test(test_zstd_levels);

	if (!j.fn) {
		pr_err("unknown compress test %s", testname);
		return -EINVAL;
	}

	int ret = j.fn(j.c, j.nr);
	if (ret)
		bch_err(c, "compress test %s failed: %s", testname, bch2_err_str(ret));
	else
		pr_info("compress test %s passed", testname);
	return ret;
}

#endif /* CONFIG_BCACHEFS_TESTS */
