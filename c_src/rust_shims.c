// SPDX-License-Identifier: GPL-2.0

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>

#include "libbcachefs.h"
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
#include "libbcachefs/btree/iter.h"
#include "libbcachefs/init/error.h"
#include "libbcachefs/init/fs.h"
#include "libbcachefs/journal/journal.h"
#include "libbcachefs/sb/clean.h"
#include "posix_to_bcachefs.h"
#include "rust_shims.h"
#include "src/rust_to_c.h"

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

struct bch_csum rust_csum_vstruct_sb(struct bch_sb *sb)
{
	struct nonce nonce = { 0 };

	return csum_vstruct(NULL, BCH_SB_CSUM_TYPE(sb), nonce, sb);
}

size_t rust_sizeof_bucket(void)
{
	return sizeof(struct bucket);
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

void strip_fs_alloc(struct bch_fs *c)
{
	struct bch_sb_field_clean *clean = bch2_sb_field_get(c->disk_sb.sb, clean);
	struct jset_entry *entry = clean->start;

	unsigned u64s = clean->field.u64s;
	while (entry != vstruct_end(&clean->field)) {
		if (entry->type == BCH_JSET_ENTRY_btree_root &&
		    btree_id_is_alloc(entry->btree_id)) {
			clean->field.u64s -= jset_u64s(entry->u64s);
			memmove(entry,
				vstruct_next(entry),
				vstruct_end(&clean->field) - (void *) vstruct_next(entry));
		} else {
			entry = vstruct_next(entry);
		}
	}

	swap(u64s, clean->field.u64s);
	bch2_sb_field_resize(&c->disk_sb, clean, u64s);

	scoped_guard(percpu_write, &c->capacity.mark_lock) {
		kfree(c->replicas.entries);
		c->replicas.entries = NULL;
		c->replicas.nr = 0;
	}

	bch2_sb_field_resize(&c->disk_sb, replicas_v0, 0);
	bch2_sb_field_resize(&c->disk_sb, replicas, 0);

	for_each_online_member(c, ca, 0) {
		bch2_sb_field_resize(&c->disk_sb, journal, 0);
		bch2_sb_field_resize(&c->disk_sb, journal_v2, 0);
	}

	for_each_member_device(c, ca) {
		struct bch_member *m = bch2_members_v2_get_mut(c->disk_sb.sb, ca->dev_idx);
		SET_BCH_MEMBER_FREESPACE_INITIALIZED(m, false);
	}

	c->disk_sb.sb->features[0] |= cpu_to_le64(BIT_ULL(BCH_FEATURE_no_alloc_info));
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

/* dump sanitize shims — wraps magic computation and crypto operations */

__u64 rust_jset_magic(struct bch_fs *c)
{
	return __jset_magic(c->disk_sb.sb);
}

__u64 rust_bset_magic(struct bch_fs *c)
{
	return __bset_magic(c->disk_sb.sb);
}

int rust_jset_decrypt(struct bch_fs *c, struct jset *j)
{
	return bch2_encrypt(c, JSET_CSUM_TYPE(j), journal_nonce(j),
			    j->encrypted_start,
			    vstruct_end(j) - (void *) j->encrypted_start);
}

int rust_bset_decrypt(struct bch_fs *c, struct bset *i, unsigned offset)
{
	return bset_encrypt(c, i, offset);
}

/* superblock display — wraps member iteration with device name lookup */

static struct sb_name *sb_dev_to_name(sb_names sb_names, unsigned idx)
{
	darray_for_each(sb_names, i)
		if (i->sb.sb->dev_idx == idx)
			return i;
	return NULL;
}

static void print_one_member(struct printbuf *out, sb_names sb_names,
			     struct bch_sb *sb,
			     struct bch_sb_field_disk_groups *gi,
			     struct bch_member m, unsigned idx)
{
	if (!bch2_member_alive(&m))
		return;

	struct sb_name *name = sb_dev_to_name(sb_names, idx);
	prt_printf(out, "Device %u:\t%s\t", idx, name ? name->name : "(not found)");

	if (name) {
		char *model = fd_to_dev_model(name->sb.bdev->bd_fd);
		prt_str(out, model);
		free(model);
	}
	prt_newline(out);

	printbuf_indent_add(out, 2);
	bch2_member_to_text(out, &m, gi, sb, idx);
	printbuf_indent_sub(out, 2);
}

void bch2_sb_to_text_with_names(struct printbuf *out,
				struct bch_fs *c, struct bch_sb *sb,
				bool print_layout, unsigned fields, int field_only)
{
	CLASS(printbuf, uuid_buf)();
	prt_str(&uuid_buf, "UUID=");
	pr_uuid(&uuid_buf, sb->user_uuid.b);

	sb_names sb_names = {};
	bch2_scan_device_sbs(uuid_buf.buf, &sb_names);

	if (field_only >= 0) {
		struct bch_sb_field *f = bch2_sb_field_get_id(sb, field_only);

		if (f)
			__bch2_sb_field_to_text(out, c, sb, f);
	} else {
		printbuf_tabstop_push(out, 44);

		bch2_sb_to_text(out, c, sb, print_layout,
				fields & ~(BIT(BCH_SB_FIELD_members_v1)|
					   BIT(BCH_SB_FIELD_members_v2)));

		struct bch_sb_field_disk_groups *gi = bch2_sb_field_get(sb, disk_groups);

		struct bch_sb_field_members_v1 *mi1;
		if ((fields & BIT(BCH_SB_FIELD_members_v1)) &&
		    (mi1 = bch2_sb_field_get(sb, members_v1)))
			for (unsigned i = 0; i < sb->nr_devices; i++)
				print_one_member(out, sb_names, sb, gi, bch2_members_v1_get(mi1, i), i);

		struct bch_sb_field_members_v2 *mi2;
		if ((fields & BIT(BCH_SB_FIELD_members_v2)) &&
		    (mi2 = bch2_sb_field_get(sb, members_v2)))
			for (unsigned i = 0; i < sb->nr_devices; i++)
				print_one_member(out, sb_names, sb, gi, bch2_members_v2_get(mi2, i), i);
	}
}

/* kill_btree_node — walks btree nodes and pwrites zeroes to corrupt them */

int rust_kill_btree_nodes(struct bch_fs *c,
			  struct rust_kill_node *nodes, size_t nr_nodes,
			  int dev_idx)
{
	int ret;
	void *zeroes;

	ret = posix_memalign(&zeroes, c->opts.block_size, c->opts.block_size);
	if (ret)
		return -ENOMEM;
	memset(zeroes, 0, c->opts.block_size);

	struct btree_trans *trans = bch2_trans_get(c);

	for (size_t n = 0; n < nr_nodes; n++) {
		struct rust_kill_node *i = &nodes[n];

		ret = __for_each_btree_node(trans, iter, i->btree, POS_MIN, 0, i->level, 0, b, ({
			if (b->c.level != i->level)
				continue;

			int ret2 = 0;
			if (!i->idx) {
				ret2 = 1;

				struct bkey_ptrs_c ptrs = bch2_bkey_ptrs_c(bkey_i_to_s_c(&b->key));
				bkey_for_each_ptr(ptrs, ptr) {
					if (dev_idx >= 0 && ptr->dev != dev_idx)
						continue;

					struct bch_dev *ca = bch2_dev_tryget(c, ptr->dev);
					if (!ca)
						continue;

					struct printbuf buf = PRINTBUF;
					bch2_bkey_val_to_text(&buf, c, bkey_i_to_s_c(&b->key));
					bch_info(c, "killing btree node on dev %i %s l=%u\n  %s",
						 ptr->dev,
						 bch2_btree_id_str(i->btree), i->level, buf.buf);
					printbuf_exit(&buf);

					int ret3 = pwrite(ca->disk_sb.bdev->bd_fd, zeroes,
						     c->opts.block_size, ptr->offset << 9);
					bch2_dev_put(ca);
					if (ret3 != c->opts.block_size) {
						bch_err(c, "pwrite error: expected %u got %i %s",
							c->opts.block_size, ret3, strerror(errno));
						ret2 = EXIT_FAILURE;
					}
				}
			}

			i->idx--;
			ret2;
		}));

		if (ret < 0) {
			bch_err(c, "error %i walking btree nodes", ret);
			break;
		} else if (!ret) {
			bch_err(c, "node at specified index not found");
			ret = EXIT_FAILURE;
			break;
		}
	}

	bch2_trans_put(trans);
	free(zeroes);
	return ret < 0 ? ret : 0;
}
