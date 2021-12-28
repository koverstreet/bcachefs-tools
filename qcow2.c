
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

struct qcow2_image {
	int			fd;
	u32			block_size;
	u64			*l1_table;
	u64			l1_offset;
	u32			l1_index;
	u64			*l2_table;
	u64			offset;
};

static void flush_l2(struct qcow2_image *img)
{
	if (img->l1_index != -1) {
		img->l1_table[img->l1_index] =
			cpu_to_be64(img->offset|QCOW_OFLAG_COPIED);
		xpwrite(img->fd, img->l2_table, img->block_size, img->offset,
			"qcow2 l2 table");
		img->offset += img->block_size;

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

void qcow2_write_image(int infd, int outfd, ranges *data,
		       unsigned block_size)
{
	u64 image_size = get_size(NULL, infd);
	unsigned l2_size = block_size / sizeof(u64);
	unsigned l1_size = DIV_ROUND_UP(image_size, (u64) block_size * l2_size);
	struct qcow2_hdr hdr = { 0 };
	struct qcow2_image img = {
		.fd		= outfd,
		.block_size	= block_size,
		.l2_table	= xcalloc(l2_size, sizeof(u64)),
		.l1_table	= xcalloc(l1_size, sizeof(u64)),
		.l1_index	= -1,
		.offset		= round_up(sizeof(hdr), block_size),
	};
	struct range *r;
	char *buf = xmalloc(block_size);
	u64 src_offset, dst_offset;

	assert(is_power_of_2(block_size));

	ranges_roundup(data, block_size);
	ranges_sort_merge(data);

	/* Write data: */
	darray_foreach(r, *data)
		for (src_offset = r->start;
		     src_offset < r->end;
		     src_offset += block_size) {
			dst_offset = img.offset;
			img.offset += img.block_size;

			xpread(infd, buf, block_size, src_offset);
			xpwrite(outfd, buf, block_size, dst_offset,
				"qcow2 data");

			add_l2(&img, src_offset / block_size, dst_offset);
		}

	flush_l2(&img);

	/* Write L1 table: */
	dst_offset		= img.offset;
	img.offset		+= round_up(l1_size * sizeof(u64), block_size);
	xpwrite(img.fd, img.l1_table, l1_size * sizeof(u64), dst_offset,
		"qcow2 l1 table");

	/* Write header: */
	hdr.magic		= cpu_to_be32(QCOW_MAGIC);
	hdr.version		= cpu_to_be32(QCOW_VERSION);
	hdr.block_bits		= cpu_to_be32(ilog2(block_size));
	hdr.size		= cpu_to_be64(image_size);
	hdr.l1_size		= cpu_to_be32(l1_size);
	hdr.l1_table_offset	= cpu_to_be64(dst_offset);

	memset(buf, 0, block_size);
	memcpy(buf, &hdr, sizeof(hdr));
	xpwrite(img.fd, buf, block_size, 0,
		"qcow2 header");

	free(img.l2_table);
	free(img.l1_table);
	free(buf);
}
