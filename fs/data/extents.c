// SPDX-License-Identifier: GPL-2.0
/*
 * Copyright (C) 2010 Kent Overstreet <kent.overstreet@gmail.com>
 *
 * Code for managing the extent btree and dynamically updating the writeback
 * dirty sector count.
 */

#include "bcachefs.h"

#include "alloc/buckets.h"
#include "alloc/disk_groups.h"

#include "btree/bkey_methods.h"
#include "btree/cache.h"
#include "btree/iter.h"
#include "btree/update.h"

#include "data/checksum.h"
#include "data/compress.h"
#include "data/ec/trigger.h"
#include "data/extents.h"
#include "data/reconcile/trigger.h"

#include "fs/inode.h"

#include "init/error.h"

#include "util/util.h"

static void bch2_extent_crc_pack(union bch_extent_crc *,
				 struct bch_extent_crc_unpacked,
				 enum bch_extent_entry_type);

/* Extent checksum entries: */

static union bch_extent_entry *bkey_crc_find(const struct bch_fs *c, struct bkey_i *k,
					     struct bch_extent_crc_unpacked crc)
{
	struct bkey_ptrs ptrs = bch2_bkey_ptrs(bkey_i_to_s(k));
	struct bch_extent_crc_unpacked i;
	union bch_extent_entry *entry;

	bkey_for_each_crc(&k->k, ptrs, i, entry)
		if (!bch2_crc_unpacked_cmp(i, crc))
			return entry;
	return NULL;
}

/* The smallest checksum entry type that can hold @crc: */
static enum bch_extent_entry_type bch2_extent_crc_type(struct bch_extent_crc_unpacked crc)
{
	if (bch_crc_bytes[crc.csum_type]	<= 4 &&
	    crc.uncompressed_size		<= CRC32_SIZE_MAX &&
	    crc.nonce				<= CRC32_NONCE_MAX)
		return BCH_EXTENT_ENTRY_crc32;
	if (bch_crc_bytes[crc.csum_type]	<= 10 &&
	    crc.uncompressed_size		<= CRC64_SIZE_MAX &&
	    crc.nonce				<= CRC64_NONCE_MAX)
		return BCH_EXTENT_ENTRY_crc64;
	if (bch_crc_bytes[crc.csum_type]	<= 16 &&
	    crc.uncompressed_size		<= CRC128_SIZE_MAX &&
	    crc.nonce				<= CRC128_NONCE_MAX)
		return BCH_EXTENT_ENTRY_crc128;
	BUG();
}

/*
 * We're writing another replica for this extent, so while we've got the data in
 * memory we'll be computing a new checksum for the currently live data.
 *
 * If there are other replicas we aren't moving, and they are checksummed but
 * not compressed, we can modify them to point to only the data that is
 * currently live (so that readers won't have to bounce) while we've got the
 * checksum we need:
 */
bool bch2_bkey_narrow_crc(const struct bch_fs *c,
			  struct bkey_i *k,
			  struct bch_extent_crc_unpacked old,
			  struct bch_extent_crc_unpacked new)
{
	BUG_ON(crc_is_compressed(new));
	BUG_ON(new.offset);
	BUG_ON(new.uncompressed_size != new.live_size);

	union bch_extent_entry *old_e = bkey_crc_find(c, k, old);
	if (!old_e)
		return false;

	struct bkey_ptrs ptrs = bch2_bkey_ptrs(bkey_i_to_s(k));
	union bch_extent_entry *i;

	bkey_extent_entry_for_each_from(ptrs, i, extent_entry_next(c, old_e)) {
		if (extent_entry_is_crc(i))
			break;
		if (extent_entry_is_ptr(i))
			i->ptr.offset += old.offset;
	}

	/*
	 * Narrowing advances the nonce by the old offset (encrypted extents),
	 * which may no longer fit the old entry's nonce field: replace it with
	 * a larger entry in the same position. The caller leaves room for a
	 * crc128.
	 */
	enum bch_extent_entry_type old_type = extent_entry_type(old_e);
	enum bch_extent_entry_type new_type = max(old_type, bch2_extent_crc_type(new));

	if (new_type == old_type) {
		bch2_extent_crc_pack(entry_to_crc(old_e), new, old_type);
	} else {
		union bch_extent_crc new_e;

		bch2_extent_crc_pack(&new_e, new, new_type);
		extent_entry_drop(c, bkey_i_to_s(k), old_e);
		__extent_entry_insert(c, k, old_e, to_entry(&new_e));
	}
	return true;
}

static void bch2_extent_crc_pack(union bch_extent_crc *dst,
				 struct bch_extent_crc_unpacked src,
				 enum bch_extent_entry_type type)
{
#define common_fields(_src)						\
		.type			= BIT(type),			\
		.csum_type		= _src.csum_type,		\
		.compression_type	= _src.compression_type,	\
		._compressed_size	= _src.compressed_size - 1,	\
		._uncompressed_size	= _src.uncompressed_size - 1,	\
		.offset			= _src.offset

	switch (type) {
	case BCH_EXTENT_ENTRY_crc32:
		dst->crc32		= (struct bch_extent_crc32) {
			common_fields(src),
			.csum		= (u32 __force) *((__le32 *) &src.csum.lo),
		};
		break;
	case BCH_EXTENT_ENTRY_crc64:
		dst->crc64		= (struct bch_extent_crc64) {
			common_fields(src),
			.nonce		= src.nonce,
			.csum_lo	= (u64 __force) src.csum.lo,
			.csum_hi	= (u64 __force) *((__le16 *) &src.csum.hi),
		};
		break;
	case BCH_EXTENT_ENTRY_crc128:
		dst->crc128		= (struct bch_extent_crc128) {
			common_fields(src),
			.nonce		= src.nonce,
			.csum		= src.csum,
		};
		break;
	default:
		BUG();
	}
#undef set_common_fields
}

void bch2_extent_crc_append(const struct bch_fs *c,
			    struct bkey_i *k,
			    struct bch_extent_crc_unpacked new)
{
	struct bkey_ptrs ptrs = bch2_bkey_ptrs(bkey_i_to_s(k));
	union bch_extent_crc *crc = (void *) ptrs.end;

	bch2_extent_crc_pack(crc, new, bch2_extent_crc_type(new));

	k->k.u64s += extent_entry_u64s(c, ptrs.end);

	EBUG_ON(bkey_val_u64s(&k->k) > BKEY_EXTENT_VAL_U64s_MAX);
}

/* Generic code for keys with pointers: */

static union bch_extent_entry *bkey_find_crc(const struct bch_fs *c, struct bkey_s k,
					     struct bch_extent_crc_unpacked crc)
{
	struct bkey_ptrs ptrs = bch2_bkey_ptrs(k);
	struct bch_extent_crc_unpacked i;
	union bch_extent_entry *entry;

	bkey_for_each_crc(k.k, ptrs, i, entry)
		if (!bch2_crc_unpacked_cmp(i, crc))
			return entry;

	return NULL;
}

void bch2_extent_ptr_decoded_append(const struct bch_fs *c, struct bkey_i *k,
				    struct extent_ptr_decoded *p)
{
	struct bch_extent_crc_unpacked crc = bch2_extent_crc_unpack(&k->k, NULL);
	union bch_extent_entry *pos;

	if (!bch2_crc_unpacked_cmp(crc, p->crc)) {
		pos = bch2_bkey_ptrs(bkey_i_to_s(k)).start;
	} else if ((pos = bkey_find_crc(c, bkey_i_to_s(k), p->crc))) {
		pos = extent_entry_next(c, pos);
	} else {
		bch2_extent_crc_append(c, k, p->crc);
		pos = bkey_val_end(bkey_i_to_s(k));
	}

	p->ptr.type = 1 << BCH_EXTENT_ENTRY_ptr;
	__extent_entry_insert(c, k, pos, to_entry(&p->ptr));

	if (p->has_ec) {
		p->ec.type = 1 << BCH_EXTENT_ENTRY_stripe_ptr;
		__extent_entry_insert(c, k, pos, to_entry(&p->ec));
	}
}

const char * const bch2_extent_entry_types[] = {
#define x(t, n, ...) [n] = #t,
	BCH_EXTENT_ENTRY_TYPES()
#undef x
	NULL
};
