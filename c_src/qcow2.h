#ifndef _QCOW2_H
#define _QCOW2_H

#include <linux/types.h>
#include "tools-util.h"

struct qcow2_image {
	int			infd;
	int			outfd;
	u64			image_size;
	u32			block_size;
	u32			l1_size;
	u64			*l1_table;
	u64			l1_offset;
	u32			l1_index;
	u64			*l2_table;
	u64			offset;
};

void qcow2_write_buf(struct qcow2_image *, void *, unsigned, u64);
void qcow2_write_ranges(struct qcow2_image *, ranges *);

void qcow2_image_init(struct qcow2_image *, int, int, unsigned);
void qcow2_image_finish(struct qcow2_image *);

void qcow2_write_image(int, int, ranges *, unsigned);

#endif /* _QCOW2_H */
