#include <ctype.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <linux/mm.h>

#include "libbcachefs.h"
#include "tools-util.h"

#include "bcachefs.h"

#include "alloc/buckets.h"
#include "btree/cache.h"

#include "sb/io.h"

void bch2_sb_layout_init(struct bch_sb_layout *l,
			 unsigned block_size,
			 unsigned bucket_size,
			 unsigned sb_size,
			 u64 sb_start, u64 sb_end,
			 bool no_sb_at_end)
{
	u64 sb_pos = sb_start;
	unsigned i;

	memset(l, 0, sizeof(*l));

	l->magic		= BCHFS_MAGIC;
	l->layout_type		= 0;
	l->nr_superblocks	= 2;
	l->sb_max_size_bits	= ilog2(sb_size);

	/* Create two superblocks in the allowed range: */
	for (i = 0; i < l->nr_superblocks; i++) {
		if (sb_pos != BCH_SB_SECTOR)
			sb_pos = round_up(sb_pos, block_size >> 9);

		l->sb_offset[i] = cpu_to_le64(sb_pos);
		sb_pos += sb_size;
	}

	if (sb_pos > sb_end)
		die("insufficient space for superblocks: start %llu end %llu > %llu size %u",
		    sb_start, sb_pos, sb_end, sb_size);

	/*
	 * Also create a backup superblock at the end of the disk:
	 *
	 * If we're not creating a superblock at the default offset, it
	 * means we're being run from the migrate tool and we could be
	 * overwriting existing data if we write to the end of the disk:
	 */
	if (sb_start == BCH_SB_SECTOR && !no_sb_at_end) {
		u64 backup_sb = sb_end - (1 << l->sb_max_size_bits);

		backup_sb = rounddown(backup_sb, bucket_size >> 9);
		l->sb_offset[l->nr_superblocks++] = cpu_to_le64(backup_sb);
	}
}

u64 bch2_pick_bucket_size(struct bch_opts opts, dev_opts_list devs)
{
	/* Hard minimum: bucket must hold a btree node */
	u64 bucket_size = opts.block_size;
	if (opt_defined(opts, btree_node_size))
		bucket_size = max_t(u64, bucket_size, opts.btree_node_size);

	u64 min_dev_size = BCH_MIN_NR_NBUCKETS * bucket_size;
	darray_for_each(devs, i)
		if (i->fs_size < min_dev_size)
			die("cannot format %s, too small (%llu bytes, min %llu)",
			    i->path, i->fs_size, min_dev_size);

	u64 total_fs_size = 0;
	darray_for_each(devs, i)
		total_fs_size += i->fs_size;

	/*
	 * Soft preferences below — these set the ideal bucket size,
	 * but dev_bucket_size_clamp() may reduce per-device to keep
	 * bucket counts reasonable on small devices:
	 */

	/* btree_node_size isn't calculated yet; use a reasonable floor: */
	bucket_size = max(bucket_size, 256ULL << 10);

	/*
	 * Avoid fragmenting encoded (checksummed/compressed) extents
	 * when they're moved — prefer buckets large enough for several
	 * max-size extents:
	 */
	bucket_size = max(bucket_size, (u64) opt_get(opts, encoded_extent_max) * 4);

	/*
	 * Prefer larger buckets up to 2MB — reduces allocator overhead.
	 * Scales linearly with total filesystem size, reaching 2MB at 2TB:
	 */
	u64 perf_lower_bound = min(2ULL << 20, total_fs_size / (1ULL << 20));
	bucket_size = max(bucket_size, perf_lower_bound);

	/*
	 * Upper bound on bucket count: ensure we can fsck with available
	 * memory.  Large fudge factor to allow for other fsck processes
	 * and devices being added after creation:
	 */
	struct sysinfo info;
	si_meminfo(&info);
	u64 mem_available_for_fsck = info.totalram / 8;
	u64 buckets_can_fsck = mem_available_for_fsck / (sizeof(struct bucket) * 1.5);
	u64 mem_lower_bound = roundup_pow_of_two(total_fs_size / buckets_can_fsck);
	bucket_size = max(bucket_size, mem_lower_bound);

	bucket_size = roundup_pow_of_two(bucket_size);

	return bucket_size;
}

void bch2_check_bucket_size(struct bch_opts opts, struct dev_opts *dev)
{
	if (dev->opts.bucket_size < opts.block_size)
		die("Bucket size (%u) cannot be smaller than block size (%u)",
		    dev->opts.bucket_size, opts.block_size);

	if (opt_defined(opts, btree_node_size) &&
	    dev->opts.bucket_size < opts.btree_node_size)
		die("Bucket size (%u) cannot be smaller than btree node size (%u)",
		    dev->opts.bucket_size, opts.btree_node_size);

	if (dev->nbuckets < BCH_MIN_NR_NBUCKETS)
		die("Not enough buckets: %llu, need %u (bucket size %u)",
		    dev->nbuckets, BCH_MIN_NR_NBUCKETS, dev->opts.bucket_size);
}

/* option parsing */

#include <getopt.h>

void bch2_opt_strs_free(struct bch_opt_strs *opts)
{
	unsigned i;

	for (i = 0; i < bch2_opts_nr; i++) {
		free(opts->by_id[i]);
		opts->by_id[i] = NULL;
	}
}

static bool opt_type_filter(const struct bch_option *opt, unsigned opt_types)
{
	if (!(opt->flags & opt_types))
		return false;

	if ((opt_types & OPT_FORMAT) &&
	    !opt->set_sb && !opt->set_member)
		return false;

	return true;
}

const struct bch_option *bch2_cmdline_opt_parse(int argc, char *argv[],
						unsigned opt_types)
{
	if (optind >= argc)
		return NULL;

	if (argv[optind][0] != '-' ||
	    argv[optind][1] != '-')
		return NULL;

	char *optstr = strdup(argv[optind] + 2);
	optarg = NULL;

	char *eq = strchr(optstr, '=');
	if (eq) {
		*eq = '\0';
		optarg = eq + 1;
	}

	int optid = bch2_opt_lookup(optstr);
	if (optid < 0)
		goto noopt;

	const struct bch_option *opt = bch2_opt_table + optid;
	if (!opt_type_filter(opt, opt_types))
		goto noopt;

	optind++;

	if (!optarg) {
		if (opt->type != BCH_OPT_BOOL)
			optarg = argv[optind++];
		else
			optarg = "1";
	}

	return opt;
noopt:
	free(optstr);
	return NULL;
}

struct bch_opt_strs bch2_cmdline_opts_get(int *argc, char *argv[],
					  unsigned opt_types)
{
	struct bch_opt_strs opts;
	unsigned i = 1;

	memset(&opts, 0, sizeof(opts));

	while (i < *argc) {
		char *optstr = strcmp_prefix(argv[i], "--");
		char *valstr = NULL, *p;
		int optid, nr_args = 1;

		if (!optstr) {
			i++;
			continue;
		}

		optstr = strdup(optstr);

		p = optstr;
		while (isalpha(*p) || *p == '_')
			p++;

		if (*p == '=') {
			*p = '\0';
			valstr = p + 1;
		}

		optid = bch2_opt_lookup(optstr);
		if (optid < 0 ||
		    !(bch2_opt_table[optid].flags & opt_types)) {
			i++;
			goto next;
		}

		if (!valstr &&
		    bch2_opt_table[optid].type != BCH_OPT_BOOL) {
			nr_args = 2;
			valstr = argv[i + 1];
		}

		if (!valstr)
			valstr = "1";

		opts.by_id[optid] = strdup(valstr);

		*argc -= nr_args;
		memmove(&argv[i],
			&argv[i + nr_args],
			sizeof(char *) * (*argc - i));
		argv[*argc] = NULL;
next:
		free(optstr);
	}

	return opts;
}

struct bch_opts bch2_parse_opts(struct bch_opt_strs strs)
{
	struct bch_opts opts = bch2_opts_empty();
	struct printbuf err = PRINTBUF;
	unsigned i;
	int ret;
	u64 v;

	for (i = 0; i < bch2_opts_nr; i++) {
		if (!strs.by_id[i])
			continue;

		ret = bch2_opt_parse(NULL,
				     &bch2_opt_table[i],
				     strs.by_id[i], &v, &err);
		if (ret < 0 && ret != -BCH_ERR_option_needs_open_fs)
			die("Invalid option %s", err.buf);

		bch2_opt_set_by_id(&opts, i, v);
	}

	printbuf_exit(&err);
	return opts;
}

#define newline(c)		\
	do {			\
		printf("\n");	\
		c = 0;		\
	} while(0)
void bch2_opts_usage(unsigned opt_types)
{
	const struct bch_option *opt;
	unsigned i, c = 0, helpcol = 32;

	for (opt = bch2_opt_table;
	     opt < bch2_opt_table + bch2_opts_nr;
	     opt++) {
		if (!opt_type_filter(opt, opt_types))
			continue;

		c += printf("      --%s", opt->attr.name);

		switch (opt->type) {
		case BCH_OPT_BOOL:
			break;
		case BCH_OPT_STR:
			c += printf("=(");
			for (i = 0; opt->choices[i]; i++) {
				if (i)
					c += printf("|");
				c += printf("%s", opt->choices[i]);
			}
			c += printf(")");
			break;
		default:
			c += printf("=%s", opt->hint);
			break;
		}

		if (opt->help) {
			const char *l = opt->help;

			if (c > helpcol) {
				newline(c);
			}

			while (1) {
				const char *n = strchrnul(l, '\n');

				while (c < helpcol-1) {
					putchar(' ');
					c++;
				}
				printf("%.*s", (int) (n - l), l);
				newline(c);

				if (!*n)
					break;
				l = n + 1;
			}
		} else {
			newline(c);
		}
	}
}

