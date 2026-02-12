#ifndef _LIBBCACHE_H
#define _LIBBCACHE_H

#include <linux/uuid.h>
#include <stdbool.h>

#include "bcachefs.h"
#include "bcachefs_format.h"

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

	unsigned kernel_version = bcachefs_kernel_version();

	return (struct format_opts) {
		.version		= kernel_version
			? min(bcachefs_metadata_version_current, kernel_version)
			: bcachefs_metadata_version_current,
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

#endif /* _LIBBCACHE_H */
