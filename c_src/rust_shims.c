// SPDX-License-Identifier: GPL-2.0

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>

#include "libbcachefs.h"
#include "libbcachefs/journal/read.h"
#include "libbcachefs/journal/seq_blacklist.h"
#include "libbcachefs/sb/io.h"
#include "libbcachefs/sb/members.h"
#include "libbcachefs/alloc/buckets_types.h"
#include "libbcachefs/data/checksum.h"
#include "libbcachefs/btree/read.h"
#include "libbcachefs/init/error.h"
#include "libbcachefs/init/fs.h"
#include "libbcachefs/journal/journal.h"
#include "libbcachefs/sb/clean.h"
#include "posix_to_bcachefs.h"
#include "rust_shims.h"

struct bch_csum rust_csum_vstruct_sb(struct bch_sb *sb)
{
	struct nonce nonce = { 0 };

	return csum_vstruct(NULL, BCH_SB_CSUM_TYPE(sb), nonce, sb);
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

/* dump sanitize shims — wraps crypto operations for encrypted fs dumps */

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

/* Device reference shims */

struct bch_dev *rust_dev_tryget_noerror(struct bch_fs *c, unsigned dev)
{
	return bch2_dev_tryget_noerror(c, dev);
}

void rust_dev_put(struct bch_dev *ca)
{
	bch2_dev_put(ca);
}
