#ifndef _LIBBCACHE_H
#define _LIBBCACHE_H

#include <linux/uuid.h>
#include <stdbool.h>

#include "libbcachefs/bcachefs.h"
#include "libbcachefs/bcachefs_format.h"
#include "libbcachefs/bcachefs_ioctl.h"
#include "libbcachefs/inode.h"
#include "libbcachefs/opts.h"
#include "libbcachefs/vstructs.h"
#include "tools-util.h"

/* option parsing */

#define SUPERBLOCK_SIZE_DEFAULT		2048	/* 1 MB */

struct bch_opt_strs {
union {
	char			*by_id[bch2_opts_nr];
struct {
#define x(_name, ...)	char	*_name;
	BCH_OPTS()
#undef x
};
};
};

void bch2_opt_strs_free(struct bch_opt_strs *);

const struct bch_option *bch2_cmdline_opt_parse(int argc, char *argv[],
						unsigned opt_types);
void bch_remove_arg_from_argv(int *argc, char *argv[], int index);
struct bch_opt_strs bch2_cmdline_opts_get(int *, char *[], unsigned);
struct bch_opts bch2_parse_opts(struct bch_opt_strs);
void bch2_opts_usage(unsigned);

struct format_opts {
	char		*label;
	__uuid_t	uuid;
	unsigned	version;
	unsigned	superblock_size;
	bool		encrypted;
	char		*passphrase_file;
	char		*passphrase;
	char		*source;
	bool		no_sb_at_end;
};

static inline unsigned bcachefs_kernel_version(void)
{
	return !access("/sys/module/bcachefs/parameters/version", R_OK)
	    ? read_file_u64(AT_FDCWD, "/sys/module/bcachefs/parameters/version")
	    : 0;
}

static inline struct format_opts format_opts_default()
{
	/*
	 * Ensure bcachefs module is loaded so we know the supported on disk
	 * format version:
	 */
	(void)!system("modprobe bcachefs > /dev/null 2>&1");

	return (struct format_opts) {
		.version		= bcachefs_kernel_version() ?:
			bcachefs_metadata_version_current,
		.superblock_size	= SUPERBLOCK_SIZE_DEFAULT,
	};
}

struct dev_opts {
	struct file	*file;
	struct block_device *bdev;
	const char	*path;

	u64		sb_offset;
	u64		sb_end;

	u64		nbuckets;
	u64		fs_size;

	const char	*label; /* make this a bch_opt */

	struct bch_opts	opts;
};

typedef DARRAY(struct dev_opts) dev_opts_list;

static inline struct dev_opts dev_opts_default()
{
	return (struct dev_opts) { .opts = bch2_opts_empty() };
}

void bch2_sb_layout_init(struct bch_sb_layout *,
			 unsigned, unsigned, unsigned, u64, u64, bool);

u64 bch2_pick_bucket_size(struct bch_opts, dev_opts_list);
void bch2_check_bucket_size(struct bch_opts, struct dev_opts *);

struct bch_sb *bch2_format(struct bch_opt_strs,
			   struct bch_opts,
			   struct format_opts,
			   dev_opts_list devs);

int bch2_format_for_device_add(struct dev_opts *,
			       unsigned, unsigned);

void bch2_super_write(int, struct bch_sb *);
struct bch_sb *__bch2_super_read(int, u64);

/* ioctl interface: */

int bcachectl_open(void);

struct bchfs_handle {
	__uuid_t	uuid;
	int		ioctl_fd;
	int		sysfs_fd;
	int		dev_idx;
};

void bcache_fs_close(struct bchfs_handle);

int bcache_fs_open_fallible(const char *, struct bchfs_handle *);

struct bchfs_handle bcache_fs_open(const char *);
struct bchfs_handle bchu_fs_open_by_dev(const char *, int *);

int bchu_dev_path_to_idx(struct bchfs_handle, const char *);

#define errmsg_ioctl(_ioctl_fd, _ioctl_nr, _ioctl_arg)			\
({									\
	char err[8192];							\
	(_ioctl_arg)->err.msg_ptr = (ulong) err;			\
	(_ioctl_arg)->err.msg_len = sizeof(err);			\
	int ret = ioctl(_ioctl_fd, _ioctl_nr, _ioctl_arg);		\
	if (ret < 0 && errno != ENOTTY)					\
		die(#_ioctl_nr " error: %s\n%s", bch2_err_str(-errno), err);\
	ret >= 0;							\
})

static inline void bchu_disk_add(struct bchfs_handle fs, const char *dev)
{
	struct bch_ioctl_disk_v2 i = { .dev = (unsigned long) dev, };

	if (!errmsg_ioctl(fs.ioctl_fd, BCH_IOCTL_DISK_ADD_v2, &i)) {
		struct bch_ioctl_disk i = { .dev = (unsigned long) dev, };
		xioctl(fs.ioctl_fd, BCH_IOCTL_DISK_ADD, &i);
	}
}

static inline void bchu_disk_remove(struct bchfs_handle fs, unsigned dev_idx,
				    unsigned flags)
{
	struct bch_ioctl_disk_v2 i = { .flags = flags|BCH_BY_INDEX, .dev = dev_idx, };

	if (!errmsg_ioctl(fs.ioctl_fd, BCH_IOCTL_DISK_REMOVE_v2, &i)) {
		struct bch_ioctl_disk i = { .flags = flags|BCH_BY_INDEX, .dev = dev_idx, };
		xioctl(fs.ioctl_fd, BCH_IOCTL_DISK_REMOVE, &i);
	}
}

static inline void bchu_disk_online(struct bchfs_handle fs, char *dev)
{
	struct bch_ioctl_disk_v2 i = { .dev = (unsigned long) dev, };

	if (!errmsg_ioctl(fs.ioctl_fd, BCH_IOCTL_DISK_ONLINE_v2, &i)) {
		struct bch_ioctl_disk i = { .dev = (unsigned long) dev, };
		xioctl(fs.ioctl_fd, BCH_IOCTL_DISK_ONLINE, &i);
	}
}

static inline void bchu_disk_offline(struct bchfs_handle fs, unsigned dev_idx,
				     unsigned flags)
{
	struct bch_ioctl_disk_v2 i = { .flags = flags|BCH_BY_INDEX, .dev = dev_idx, };

	if (!errmsg_ioctl(fs.ioctl_fd, BCH_IOCTL_DISK_OFFLINE_v2, &i)) {
		struct bch_ioctl_disk i = { .flags = flags|BCH_BY_INDEX, .dev = dev_idx, };
		xioctl(fs.ioctl_fd, BCH_IOCTL_DISK_OFFLINE, &i);
	}
}

static inline void bchu_disk_set_state(struct bchfs_handle fs, unsigned dev,
				       unsigned new_state, unsigned flags)
{
	struct bch_ioctl_disk_set_state_v2 i = {
		.flags		= flags|BCH_BY_INDEX,
		.new_state	= new_state,
		.dev		= dev,
	};

	if (!errmsg_ioctl(fs.ioctl_fd, BCH_IOCTL_DISK_SET_STATE_v2, &i)) {
		struct bch_ioctl_disk_set_state i = {
			.flags		= flags|BCH_BY_INDEX,
			.new_state	= new_state,
			.dev		= dev,
		};

		xioctl(fs.ioctl_fd, BCH_IOCTL_DISK_SET_STATE, &i);
	}
}

static inline struct bch_ioctl_fs_usage *bchu_fs_usage(struct bchfs_handle fs)
{
	struct bch_ioctl_fs_usage *u = NULL;
	size_t replica_entries_bytes = 4096;

	while (1) {
		u = xrealloc(u, sizeof(*u) + replica_entries_bytes);
		u->replica_entries_bytes = replica_entries_bytes;

		if (!ioctl(fs.ioctl_fd, BCH_IOCTL_FS_USAGE, u))
			return u;

		if (errno != ERANGE)
			die("BCH_IOCTL_USAGE error: %m");

		replica_entries_bytes *= 2;
	}
}

static inline struct bch_ioctl_query_accounting *bchu_fs_accounting(struct bchfs_handle fs,
								    unsigned typemask)
{
	unsigned accounting_u64s = 128;
	struct bch_ioctl_query_accounting *ret = NULL;

	while (1) {
		ret = xrealloc(ret, sizeof(*ret) + accounting_u64s * sizeof(u64));

		memset(ret, 0, sizeof(*ret));

		ret->accounting_u64s = accounting_u64s;
		ret->accounting_types_mask = typemask;

		if (!ioctl(fs.ioctl_fd, BCH_IOCTL_QUERY_ACCOUNTING, ret))
			return ret;

		if (errno == ENOTTY)
			return NULL;

		if (errno == ERANGE) {
			accounting_u64s *= 2;
			continue;
		}

		die("BCH_IOCTL_USAGE error: %m");
	}
}

static inline struct bch_ioctl_dev_usage_v2 *bchu_dev_usage(struct bchfs_handle fs,
							    unsigned idx)
{
	struct bch_ioctl_dev_usage_v2 *u = xcalloc(sizeof(*u) + sizeof(u->d[0]) * BCH_DATA_NR, 1);

	u->dev			= idx;
	u->flags		= BCH_BY_INDEX;
	u->nr_data_types	= BCH_DATA_NR;

	if (!ioctl(fs.ioctl_fd, BCH_IOCTL_DEV_USAGE_V2, u))
		return u;

	struct bch_ioctl_dev_usage u_v1 = { .dev = idx, .flags = BCH_BY_INDEX};
	xioctl(fs.ioctl_fd, BCH_IOCTL_DEV_USAGE, &u_v1);

	u->state	= u_v1.state;
	u->nr_data_types = ARRAY_SIZE(u_v1.d);
	u->bucket_size	= u_v1.bucket_size;
	u->nr_buckets	= u_v1.nr_buckets;

	for (unsigned i = 0; i < ARRAY_SIZE(u_v1.d); i++)
		u->d[i] = u_v1.d[i];

	return u;
}

static inline struct bch_sb *bchu_read_super(struct bchfs_handle fs, unsigned idx)
{
	size_t size = 4096;
	struct bch_sb *sb = NULL;

	while (1) {
		sb = xrealloc(sb, size);
		struct bch_ioctl_read_super i = {
			.size	= size,
			.sb	= (unsigned long) sb,
		};

		if (idx != -1) {
			i.flags |= BCH_READ_DEV|BCH_BY_INDEX;
			i.dev = idx;
		}

		if (!ioctl(fs.ioctl_fd, BCH_IOCTL_READ_SUPER, &i))
			return sb;
		if (errno != ERANGE)
			die("BCH_IOCTL_READ_SUPER error: %m");
		size *= 2;
	}
}

static inline unsigned bchu_disk_get_idx(struct bchfs_handle fs, dev_t dev)
{
	struct bch_ioctl_disk_get_idx i = { .dev = dev };

	return xioctl(fs.ioctl_fd, BCH_IOCTL_DISK_GET_IDX, &i);
}

static inline void bchu_disk_resize(struct bchfs_handle fs,
				    unsigned idx,
				    u64 nbuckets)
{
	struct bch_ioctl_disk_resize_v2 i = {
		.flags	= BCH_BY_INDEX,
		.dev	= idx,
		.nbuckets = nbuckets,
	};

	if (!errmsg_ioctl(fs.ioctl_fd, BCH_IOCTL_DISK_RESIZE_v2, &i)) {
		struct bch_ioctl_disk_resize i = {
			.flags	= BCH_BY_INDEX,
			.dev	= idx,
			.nbuckets = nbuckets,
		};

		xioctl(fs.ioctl_fd, BCH_IOCTL_DISK_RESIZE, &i);
	}
}

static inline void bchu_disk_resize_journal(struct bchfs_handle fs,
					    unsigned idx,
					    u64 nbuckets)
{
	struct bch_ioctl_disk_resize_v2 i = {
		.flags	= BCH_BY_INDEX,
		.dev	= idx,
		.nbuckets = nbuckets,
	};

	if (!errmsg_ioctl(fs.ioctl_fd, BCH_IOCTL_DISK_RESIZE_JOURNAL_v2, &i)) {
		struct bch_ioctl_disk_resize i = {
			.flags	= BCH_BY_INDEX,
			.dev	= idx,
			.nbuckets = nbuckets,
		};

		xioctl(fs.ioctl_fd, BCH_IOCTL_DISK_RESIZE_JOURNAL, &i);
	}
}

int bchu_data(struct bchfs_handle, struct bch_ioctl_data);

struct dev_name {
	unsigned		idx;
	char			*dev;
	char			*label;
	uuid_t			uuid;
	unsigned		durability;
	enum bch_member_state	state;
};
typedef DARRAY(struct dev_name) dev_names;

dev_names bchu_fs_get_devices(struct bchfs_handle);
struct dev_name *dev_idx_to_name(dev_names *dev_names, unsigned idx);

#endif /* _LIBBCACHE_H */
