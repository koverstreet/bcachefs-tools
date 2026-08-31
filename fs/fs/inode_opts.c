// SPDX-License-Identifier: GPL-2.0
/*
 * Per-inode io path options: how they are stored on the inode, resolved
 * against filesystem defaults, and inherited.
 *
 * Storage is the awkward part everything else is built on: an option is held
 * in the inode with a +1 bias, so 0 means "not set, inherit the filesystem
 * default" and 1 means an explicit "none". Nothing but this file should be
 * reaching into bi_<option> directly.
 */

#include "bcachefs.h"

#include "data/compress.h"

#include "fs/inode.h"
#include "fs/inode_opts.h"

#define x(name, ...)	#name,
const char * const bch2_inode_opts[] = {
	BCH_INODE_OPTS()
	NULL,
};
#undef x

int bch2_opt_to_inode_opt(int id)
{
	switch (id) {
#define x(name, ...)				\
	case Opt_##name: return Inode_opt_##name;
	BCH_INODE_OPTS()
#undef  x
	default:
		return -1;
	}
}

/*
 * Copy the options @dst inherits from @src - everything it hasn't set for
 * itself. Called when a file moves between directories.
 */
bool bch2_reinherit_attrs(struct bch_inode_unpacked *dst_u,
			  struct bch_inode_unpacked *src_u)
{
	bool ret = false;

	for (unsigned id = 0; id < Inode_opt_nr; id++) {
		if (!S_ISDIR(dst_u->bi_mode) && id == Inode_opt_casefold)
			continue;

		if (dst_u->bi_fields_set & (1 << id))
			continue;

		u64 src = bch2_inode_opt_get(src_u, id);
		u64 dst = bch2_inode_opt_get(dst_u, id);

		if (src == dst)
			continue;

		bch2_inode_opt_set(dst_u, id, src);
		ret = true;
	}

	return ret;
}

struct bch_opts bch2_inode_opts_to_opts(struct bch_inode_unpacked *inode)
{
	struct bch_opts ret = { 0 };
#define x(_name, _bits)							\
	if (inode->bi_##_name)						\
		opt_set(ret, _name, inode->bi_##_name - 1);
	BCH_INODE_OPTS()
#undef x
	return ret;
}

void bch2_inode_opts_get_inode(struct bch_fs *c,
			       struct bch_inode_unpacked *inode,
			       struct bch_inode_opts *ret)
{
#define x(_name, _bits)							\
	if ((inode)->bi_##_name) {					\
		ret->_name = inode->bi_##_name - 1;			\
		ret->_name##_from_inode = true;				\
	} else {							\
		ret->_name = c->opts._name;				\
		ret->_name##_from_inode = false;			\
	}
	BCH_INODE_OPTS()
#undef x

	/*
	 * Forward compatibility: inodes written by newer versions may carry
	 * checksum/compression types we don't know about — fall back to the
	 * filesystem option for new writes. Reads are unaffected, extents
	 * carry their own types. (This is why these aren't validated at
	 * btree read time: that would reject valid inodes from newer
	 * versions.)
	 */
	if (unlikely(ret->data_checksum >= BCH_CSUM_OPT_NR)) {
		ret->data_checksum = c->opts.data_checksum;
		ret->data_checksum_from_inode = false;
	}
	if (unlikely(!bch2_compression_opt_valid(ret->compression))) {
		ret->compression = c->opts.compression;
		ret->compression_from_inode = false;
	}
	if (unlikely(!bch2_compression_opt_valid(ret->background_compression))) {
		ret->background_compression = c->opts.background_compression;
		ret->background_compression_from_inode = false;
	}

	ret->change_cookie = c->opt_change_cookie;

	bch2_io_opts_fixups(ret);
}
