// SPDX-License-Identifier: GPL-2.0

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>

#include "libbcachefs.h"
#include "libbcachefs/opts.h"
#include "libbcachefs/btree/cache.h"
#include "libbcachefs/init/dev.h"
#include "libbcachefs/journal/init.h"
#include "libbcachefs/journal/read.h"
#include "libbcachefs/journal/seq_blacklist.h"
#include "libbcachefs/sb/io.h"
#include "libbcachefs/sb/members.h"
#include "libbcachefs/alloc/buckets_types.h"
#include "libbcachefs/data/checksum.h"
#include "libbcachefs/data/extents.h"
#include "libbcachefs/btree/read.h"
#include "libbcachefs/fs/dirent_format.h"
#include "libbcachefs/init/error.h"
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

size_t rust_sizeof_bucket(void)
{
	return sizeof(struct bucket);
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

/* btree node introspection shims */

bool rust_btree_node_fake(struct btree *b)
{
	return btree_node_fake(b);
}

struct btree *rust_btree_id_root_b(struct bch_fs *c, unsigned id)
{
	struct btree_root *r = bch2_btree_id_root(c, id);
	return r ? r->b : NULL;
}

unsigned rust_btree_id_nr_alive(struct bch_fs *c)
{
	return btree_id_nr_alive(c);
}

/* online member iteration shim */

struct bch_dev *rust_get_next_online_dev(struct bch_fs *c,
					 struct bch_dev *ca,
					 unsigned ref_idx)
{
	return bch2_get_next_online_dev(c, ca, ~0U, READ, ref_idx);
}

void rust_put_online_dev_ref(struct bch_dev *ca, unsigned ref_idx)
{
	enumerated_ref_put(&ca->io_ref[READ], ref_idx);
}

struct rust_journal_entries rust_collect_journal_entries(struct bch_fs *c)
{
	struct rust_journal_entries ret = { NULL, 0 };
	struct genradix_iter iter;
	struct journal_replay **_p;
	size_t count = 0;

	genradix_for_each(&c->journal_entries, iter, _p)
		if (*_p)
			count++;

	if (!count)
		return ret;

	ret.entries = malloc(count * sizeof(*ret.entries));
	if (!ret.entries)
		die("malloc");

	genradix_for_each(&c->journal_entries, iter, _p)
		if (*_p)
			ret.entries[ret.nr++] = *_p;

	return ret;
}

/* dump sanitize shims — wraps vstruct iteration + encryption macros */

static void sanitize_key(struct bkey_packed *k, struct bkey_format *f, void *end,
			 bool sanitize_filenames, bool *modified)
{
	struct bch_val *v = bkeyp_val(f, k);
	unsigned len = min_t(unsigned, end - (void *) v, bkeyp_val_bytes(f, k));

	switch (k->type) {
	case KEY_TYPE_inline_data: {
		struct bch_inline_data *d = container_of(v, struct bch_inline_data, v);

		memset(&d->data[0], 0, len - offsetof(struct bch_inline_data, data));
		*modified = true;
		break;
	}
	case KEY_TYPE_indirect_inline_data: {
		struct bch_indirect_inline_data *d = container_of(v, struct bch_indirect_inline_data, v);

		memset(&d->data[0], 0, len - offsetof(struct bch_indirect_inline_data, data));
		*modified = true;
		break;
	}

	case KEY_TYPE_dirent:
		if (sanitize_filenames) {
			struct bch_dirent *d = container_of(v, struct bch_dirent, v);

			memset(d->d_name, 'X', len - offsetof(struct bch_dirent, d_name));
			*modified = true;
		}
	}
}

void rust_sanitize_journal(struct bch_fs *c, void *buf, size_t len,
			   bool sanitize_filenames)
{
	struct bkey_format f = BKEY_FORMAT_CURRENT;
	void *end = buf + len;

	while (len) {
		struct jset *j = buf;
		bool modified = false;

		if (le64_to_cpu(j->magic) != jset_magic(c))
			break;

		if (bch2_csum_type_is_encryption(JSET_CSUM_TYPE(j))) {
			if (!c->chacha20_key_set) {
				fprintf(stderr,
					"found encrypted journal entry on non-encrypted filesystem\n");
				return;
			}

			if (vstruct_bytes(j) > len) {
				fprintf(stderr,
					"encrypted journal entry overruns bucket; skipping\n");
				return;
			}

			int ret = bch2_encrypt(c, JSET_CSUM_TYPE(j), journal_nonce(j),
					       j->encrypted_start,
					       vstruct_end(j) - (void *) j->encrypted_start);
			if (ret)
				die("error decrypting journal entry: %s", bch2_err_str(ret));

			modified = true;
		}

		vstruct_for_each(j, i) {
			if ((void *) i >= end)
				break;

			if (!jset_entry_is_key(i))
				continue;

			jset_entry_for_each_key(i, k) {
				if ((void *) k >= end)
					break;
				if (!k->k.u64s)
					break;
				sanitize_key(bkey_to_packed(k), &f, end, sanitize_filenames, &modified);
			}
		}

		if (modified) {
			memset(&j->csum, 0, sizeof(j->csum));
			SET_JSET_CSUM_TYPE(j, 0);
		}

		unsigned b = min(len, vstruct_sectors(j, c->block_bits) << 9);
		len -= b;
		buf += b;
	}
}

void rust_sanitize_btree(struct bch_fs *c, void *buf, size_t len,
			 bool sanitize_filenames)
{
	void *end = buf + len;
	bool first = true;
	struct bkey_format f_current = BKEY_FORMAT_CURRENT;
	struct bkey_format f;
	unsigned offset = 0;
	u64 seq;

	while (len) {
		unsigned sectors;
		struct bset *i;
		bool modified = false;

		if (first) {
			struct btree_node *bn = buf;

			if (le64_to_cpu(bn->magic) != bset_magic(c))
				break;

			i = &bn->keys;
			seq = bn->keys.seq;
			f = bn->format;

			sectors = vstruct_sectors(bn, c->block_bits);
		} else {
			struct btree_node_entry *bne = buf;

			if (bne->keys.seq != seq)
				break;

			i = &bne->keys;
			sectors = vstruct_sectors(bne, c->block_bits);
		}

		if (bch2_csum_type_is_encryption(BSET_CSUM_TYPE(i))) {
			if (!c->chacha20_key_set) {
				fprintf(stderr,
					"found encrypted btree node on non-encrypted filesystem\n");
				return;
			}

			if (vstruct_end(i) > end) {
				fprintf(stderr,
					"encrypted btree node entry overruns bucket; skipping\n");
				return;
			}

			int ret = bset_encrypt(c, i, offset);
			if (ret)
				die("error decrypting btree node: %s", bch2_err_str(ret));

			modified = true;
		}

		vstruct_for_each(i, k) {
			if ((void *) k >= end)
				break;
			if (!k->u64s)
				break;

			sanitize_key(k, bkey_packed(k) ? &f : &f_current, end,
				     sanitize_filenames, &modified);
		}

		if (modified) {
			if (first) {
				struct btree_node *bn = buf;
				memset(&bn->csum, 0, sizeof(bn->csum));
			} else {
				struct btree_node_entry *bne = buf;
				memset(&bne->csum, 0, sizeof(bne->csum));
			}
			SET_BSET_CSUM_TYPE(i, 0);
		}

		first = false;

		unsigned b = min(len, sectors << 9);
		len -= b;
		buf += b;
		offset += b;
	}
}
