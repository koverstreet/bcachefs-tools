// SPDX-License-Identifier: GPL-2.0

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>

#include "libbcachefs.h"
#include "libbcachefs/opts.h"
#include "libbcachefs/sb/io.h"
#include "cmd_strip_alloc.h"
#include "posix_to_bcachefs.h"
#include "rust_shims.h"

char *rust_opts_usage_to_str(unsigned flags_all, unsigned flags_none)
{
	char *buf = NULL;
	size_t size = 0;
	FILE *memf = open_memstream(&buf, &size);
	if (!memf)
		return NULL;

	FILE *saved = stdout;
	stdout = memf;

	const struct bch_option *opt;
	unsigned c = 0, helpcol = 32;
	for (opt = bch2_opt_table;
	     opt < bch2_opt_table + bch2_opts_nr;
	     opt++) {
		if ((opt->flags & flags_all) != flags_all)
			continue;
		if (opt->flags & flags_none)
			continue;

		c += printf("      --%s", opt->attr.name);

		switch (opt->type) {
		case BCH_OPT_BOOL:
			break;
		case BCH_OPT_STR:
			c += printf("=(");
			for (unsigned i = 0; opt->choices[i]; i++) {
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
				printf("\n");
				c = 0;
			}

			while (1) {
				const char *n = strchrnul(l, '\n');

				while (c < helpcol - 1) {
					putchar(' ');
					c++;
				}
				printf("%.*s", (int)(n - l), l);
				printf("\n");
				c = 0;

				if (!*n)
					break;
				l = n + 1;
			}
		} else {
			printf("\n");
			c = 0;
		}
	}

	stdout = saved;
	fclose(memf);
	return buf;
}

int rust_fmt_build_fs(struct bch_fs *c, const char *src_path)
{
	struct copy_fs_state s = {};
	int src_fd = open(src_path, O_RDONLY|O_NOATIME);
	if (src_fd < 0)
		return -errno;

	int ret = copy_fs(c, &s, src_fd, src_path);
	close(src_fd);
	return ret;
}

int rust_strip_alloc_check(struct bch_fs *c)
{
	if (!c->sb.clean)
		return 1;

	u64 capacity = 0;
	for_each_member_device(c, ca)
		capacity += ca->mi.nbuckets * (ca->mi.bucket_size << 9);

	if (capacity > 1ULL << 40)
		return -ERANGE;

	return 0;
}

void rust_strip_alloc_do(struct bch_fs *c)
{
	mutex_lock(&c->sb_lock);
	strip_fs_alloc(c);
	bch2_write_super(c);
	mutex_unlock(&c->sb_lock);
}
