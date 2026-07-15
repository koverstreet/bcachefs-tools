#ifndef _LINUX_BACKING_DEV_H
#define _LINUX_BACKING_DEV_H

#include <linux/list.h>

typedef int (congested_fn)(void *, int);

enum wb_congested_state {
	WB_async_congested,	/* The async (write) queue is getting full */
	WB_sync_congested,	/* The sync queue is getting full */
};

struct backing_dev_info {
	struct list_head bdi_list;
	unsigned	ra_pages;
	unsigned	capabilities;

	congested_fn	*congested_fn;
	void		*congested_data;
};

#define BDI_CAP_NO_ACCT_DIRTY	0x00000001
#define BDI_CAP_NO_WRITEBACK	0x00000002
#define BDI_CAP_NO_ACCT_WB	0x00000004
#define BDI_CAP_STABLE_WRITES	0x00000008
#define BDI_CAP_STRICTLIMIT	0x00000010
#define BDI_CAP_CGROUP_WRITEBACK 0x00000020

static inline int bdi_congested(struct backing_dev_info *bdi, int cong_bits)
{
	return 0;
}

static inline int bdi_setup_and_register(struct backing_dev_info *bdi, char *name)
{
	bdi->capabilities = 0;
	return 0;
}

static inline void bdi_destroy(struct backing_dev_info *bdi) {}

#define VM_MAX_READAHEAD	128	/* kbytes */

#endif	/* _LINUX_BACKING_DEV_H */
