// SPDX-License-Identifier: GPL-2.0

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>

#include "libbcachefs.h"
#include "libbcachefs/opts.h"
#include "libbcachefs/init/dev.h"
#include "libbcachefs/journal/init.h"
#include "libbcachefs/sb/io.h"
#include "libbcachefs/sb/members.h"
#include "libbcachefs/data/checksum.h"
#include "cmd_strip_alloc.h"
#include "posix_to_bcachefs.h"
#include "rust_shims.h"

/* LE64_BITMASK setter shims for Rust — wraps static inline SET_* macros */

void rust_set_bch_sb_version_incompat_allowed(struct bch_sb *sb, __u64 v)
{ SET_BCH_SB_VERSION_INCOMPAT_ALLOWED(sb, v); }

void rust_set_bch_sb_meta_replicas_req(struct bch_sb *sb, __u64 v)
{ SET_BCH_SB_META_REPLICAS_REQ(sb, v); }

void rust_set_bch_sb_data_replicas_req(struct bch_sb *sb, __u64 v)
{ SET_BCH_SB_DATA_REPLICAS_REQ(sb, v); }

void rust_set_bch_sb_extent_bp_shift(struct bch_sb *sb, __u64 v)
{ SET_BCH_SB_EXTENT_BP_SHIFT(sb, v); }

void rust_set_bch_sb_foreground_target(struct bch_sb *sb, __u64 v)
{ SET_BCH_SB_FOREGROUND_TARGET(sb, v); }

void rust_set_bch_sb_background_target(struct bch_sb *sb, __u64 v)
{ SET_BCH_SB_BACKGROUND_TARGET(sb, v); }

void rust_set_bch_sb_promote_target(struct bch_sb *sb, __u64 v)
{ SET_BCH_SB_PROMOTE_TARGET(sb, v); }

void rust_set_bch_sb_metadata_target(struct bch_sb *sb, __u64 v)
{ SET_BCH_SB_METADATA_TARGET(sb, v); }

void rust_set_bch_sb_encryption_type(struct bch_sb *sb, __u64 v)
{ SET_BCH_SB_ENCRYPTION_TYPE(sb, v); }

void rust_set_bch_member_rotational_set(struct bch_member *m, __u64 v)
{ SET_BCH_MEMBER_ROTATIONAL_SET(m, v); }

void rust_set_bch_member_group(struct bch_member *m, __u64 v)
{ SET_BCH_MEMBER_GROUP(m, v); }

__u64 rust_bch_sb_features_all(void)
{ return BCH_SB_FEATURES_ALL; }

struct bch_csum rust_csum_vstruct_sb(struct bch_sb *sb)
{
	struct nonce nonce = { 0 };

	return csum_vstruct(NULL, BCH_SB_CSUM_TYPE(sb), nonce, sb);
}

size_t rust_vstruct_bytes_sb(const struct bch_sb *sb)
{
	return vstruct_bytes(sb);
}

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

void rust_device_set_state_offline(struct bch_fs *c,
				   unsigned dev_idx, unsigned new_state)
{
	mutex_lock(&c->sb_lock);
	struct bch_member *m = bch2_members_v2_get_mut(c->disk_sb.sb, dev_idx);
	SET_BCH_MEMBER_STATE(m, new_state);
	bch2_write_super(c);
	mutex_unlock(&c->sb_lock);
}

int rust_device_resize_offline(struct bch_fs *c, u64 size)
{
	struct bch_dev *resize = NULL;

	for_each_online_member(c, ca, 0) {
		if (resize) {
			enumerated_ref_put(&resize->io_ref[READ], 0);
			return -EINVAL;
		}
		resize = ca;
		enumerated_ref_get(&resize->io_ref[READ], 0);
	}
	if (!resize)
		return -ENODEV;

	u64 nbuckets = size / resize->mi.bucket_size;

	if (nbuckets < le64_to_cpu(resize->mi.nbuckets)) {
		enumerated_ref_put(&resize->io_ref[READ], 0);
		return -ENOSPC;
	}

	printf("resizing to %llu buckets\n", nbuckets);
	CLASS(printbuf, err)();
	int ret = bch2_dev_resize(c, resize, nbuckets, &err);
	if (ret)
		fprintf(stderr, "resize error: %s\n%s", bch2_err_str(ret), err.buf);

	enumerated_ref_put(&resize->io_ref[READ], 0);
	return ret;
}

int rust_device_resize_journal_offline(struct bch_fs *c, u64 size)
{
	struct bch_dev *resize = NULL;

	for_each_online_member(c, ca, 0) {
		if (resize) {
			enumerated_ref_put(&resize->io_ref[READ], 0);
			return -EINVAL;
		}
		resize = ca;
		enumerated_ref_get(&resize->io_ref[READ], 0);
	}
	if (!resize)
		return -ENODEV;

	u64 nbuckets = size / le16_to_cpu(resize->mi.bucket_size);

	printf("resizing journal to %llu buckets\n", nbuckets);
	int ret = bch2_set_nr_journal_buckets(c, resize, nbuckets);
	if (ret)
		fprintf(stderr, "resize error: %s\n", bch2_err_str(ret));

	enumerated_ref_put(&resize->io_ref[READ], 0);
	return ret;
}
