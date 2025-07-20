
#include <errno.h>
#include <sys/types.h>
#include <unistd.h>

#include "qcow2.h"
#include "tools-util.h"

#define QCOW_MAGIC		(('Q' << 24) | ('F' << 16) | ('I' << 8) | 0xfb)
#define QCOW_VERSION		2
#define QCOW_OFLAG_COPIED	(1LL << 63)

struct qcow2_hdr {
	u32			magic;
	u32			version;

	u64			backing_file_offset;
	u32			backing_file_size;

	u32			block_bits;
	u64			size;
	u32			crypt_method;

	u32			l1_size;
	u64			l1_table_offset;

	u64			refcount_table_offset;
	u32			refcount_table_blocks;

	u32			nb_snapshots;
	u64			snapshots_offset;
};

static void __qcow2_write_buf(struct qcow2_image *img, void *buf, unsigned len)
{
	xpwrite(img->outfd, buf, img->block_size, img->offset, "qcow2 data");
	img->offset += img->block_size;
}

static void flush_l2(struct qcow2_image *img)
{
	if (img->l1_index != -1) {
		img->l1_table[img->l1_index] =
			cpu_to_be64(img->offset|QCOW_OFLAG_COPIED);

		__qcow2_write_buf(img, img->l2_table, img->block_size);

		memset(img->l2_table, 0, img->block_size);
		img->l1_index = -1;
	}
}

static void add_l2(struct qcow2_image *img, u64 src_blk, u64 dst_offset)
{
	unsigned l2_size = img->block_size / sizeof(u64);
	u64 l1_index = src_blk / l2_size;
	u64 l2_index = src_blk & (l2_size - 1);

	if (img->l1_index != l1_index) {
		flush_l2(img);
		img->l1_index = l1_index;
	}

	img->l2_table[l2_index] = cpu_to_be64(dst_offset|QCOW_OFLAG_COPIED);
}

void qcow2_write_buf(struct qcow2_image *img, void *buf, unsigned len, u64 src_offset)
{
	u64 dst_offset = img->offset;
	__qcow2_write_buf(img, buf, len);
	add_l2(img, src_offset / img->block_size, dst_offset);
}

void qcow2_write_ranges(struct qcow2_image *img, ranges *data)
{
	ranges_roundup(data, img->block_size);
	ranges_sort_merge(data);

	char *buf = xmalloc(img->block_size);

	/* Write data: */
	darray_for_each(*data, r)
		for (u64 src_offset = r->start;
		     src_offset < r->end;
		     src_offset += img->block_size) {
			xpread(img->infd, buf, img->block_size, src_offset);
			qcow2_write_buf(img, buf, img->block_size, src_offset);
		}

	free(buf);
}

void qcow2_image_init(struct qcow2_image *img, int infd, int outfd, unsigned block_size)
{
	assert(is_power_of_2(block_size));

	u64 image_size = get_size(infd);
	unsigned l2_size = block_size / sizeof(u64);
	unsigned l1_size = DIV_ROUND_UP(image_size, (u64) block_size * l2_size);

	*img = (struct qcow2_image) {
		.infd		= infd,
		.outfd		= outfd,
		.image_size	= image_size,
		.block_size	= block_size,
		.l1_size	= l1_size,
		.l1_table	= xcalloc(l1_size, sizeof(u64)),
		.l1_index	= -1,
		.l2_table	= xcalloc(l2_size, sizeof(u64)),
		.offset		= round_up(sizeof(struct qcow2_hdr), block_size),
	};
}

void qcow2_image_finish(struct qcow2_image *img)
{
	char *buf = xmalloc(img->block_size);

	flush_l2(img);

	/* Write L1 table: */
	u64 dst_offset		= img->offset;
	img->offset		+= round_up(img->l1_size * sizeof(u64), img->block_size);
	xpwrite(img->outfd, img->l1_table, img->l1_size * sizeof(u64), dst_offset,
		"qcow2 l1 table");

	/* Write header: */
	struct qcow2_hdr hdr = {
		.magic			= cpu_to_be32(QCOW_MAGIC),
		.version		= cpu_to_be32(QCOW_VERSION),
		.block_bits		= cpu_to_be32(ilog2(img->block_size)),
		.size			= cpu_to_be64(img->image_size),
		.l1_size		= cpu_to_be32(img->l1_size),
		.l1_table_offset	= cpu_to_be64(dst_offset),
	};

	memset(buf, 0, img->block_size);
	memcpy(buf, &hdr, sizeof(hdr));
	xpwrite(img->outfd, buf, img->block_size, 0,
		"qcow2 header");

	free(img->l2_table);
	free(img->l1_table);
	free(buf);
}

void qcow2_write_image(int infd, int outfd, ranges *data,
		       unsigned block_size)
{
	struct qcow2_image img;

	qcow2_image_init(&img, infd, outfd, block_size);
	qcow2_write_ranges(&img, data);
	qcow2_image_finish(&img);
}
