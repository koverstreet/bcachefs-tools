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

#ifdef CONFIG_BCACHEFS_DEBUG
#include <linux/module.h>

static int bch2_force_read_device = -1;

module_param_named(force_read_device, bch2_force_read_device, int, 0644);
MODULE_PARM_DESC(force_read_device, "");
#endif

static void bch2_extent_crc_pack(union bch_extent_crc *,
				 struct bch_extent_crc_unpacked,
				 enum bch_extent_entry_type);

__cold void bch2_io_failures_to_text(struct printbuf *out,
			      struct bch_fs *c,
			      struct bch_io_failures *failed)
{
	scoped_guard(printbuf_indent, out)
		darray_for_each(*failed, f) {
			bch2_printbuf_make_room(out, 1024);
			scoped_guard(rcu) {
				guard(printbuf_atomic)(out);
				struct bch_dev *ca = bch2_dev_rcu_noerror(c, f->dev);
				if (ca)
					prt_str(out, ca->name);
				else
					prt_printf(out, "(invalid device %u)", f->dev);
			}

			if (!f->csum_nr && !f->ec_errcode && !f->errcode)
				prt_str(out, " no error - confused");

			if (f->csum_nr)
				prt_printf(out, " checksum (%u)", f->csum_nr);
			if (f->errcode)
				prt_printf(out, " io: %s", bch2_err_str(f->errcode));
			if (f->ec_errcode)
				prt_printf(out, " ec reconstruct: %s", bch2_err_str(f->ec_errcode));
			prt_newline(out);
		}

	if (failed->ec_msg.pos) {
		prt_printf(out, "ec reconstruct:\n");
		guard(printbuf_indent)(out);
		prt_str(out, failed->ec_msg.buf);
	}
}

/*
 * A device that's been removed said so once, when it was removed. Every write
 * that had a pointer to it saying so again is noise - so when the operation
 * itself succeeded, there's nothing here the log doesn't already have.
 *
 * Only for the soft path: the caller must have checked that it didn't fail.
 * An entry with no error at all ("no error - confused") is not covered, and
 * still reports - that one means our bookkeeping is wrong.
 */
bool bch2_io_failures_all_dev_removed(struct bch_io_failures *failed)
{
	darray_for_each(*failed, f)
		if (f->csum_nr ||
		    f->ec_errcode ||
		    !bch2_err_matches(f->errcode, BCH_ERR_BLK_STS_REMOVED))
			return false;
	return true;
}

struct bch_dev_io_failures *bch2_dev_io_failures(struct bch_io_failures *f,
						 unsigned dev)
{
	return darray_find_p(*f, i, i->dev == dev);
}

struct bch_dev_io_failures *bch2_dev_io_failures_mut(struct bch_io_failures *failed,
						     unsigned dev)
{
	struct bch_dev_io_failures *f = bch2_dev_io_failures(failed, dev);
	if (!f) {
		BUG_ON(failed->nr >= ARRAY_SIZE(failed->data));

		f = &failed->data[failed->nr++];
		memset(f, 0, sizeof(*f));
		f->dev = dev;
	}

	return f;
}

void bch2_mark_io_failure(struct bch_io_failures *failed,
			  struct extent_ptr_decoded *p, int err)
{
	BUG_ON(!err);
	BUG_ON(bch2_err_matches(err, BCH_ERR_transaction_restart));

	struct bch_dev_io_failures *f = bch2_dev_io_failures_mut(failed, p->ptr.dev);

	if (p->do_ec_reconstruct)
		f->ec_errcode = err;
	else if (err == -BCH_ERR_data_read_retry_csum_err)
		f->csum_nr++;
	else
		f->errcode = err;
}

static inline u64 dev_latency(struct bch_dev *ca)
{
	return ca ? atomic64_read(&ca->cur_latency[READ]) : S64_MAX;
}

static inline int dev_failed(struct bch_dev *ca)
{
	return !ca || ca->mi.state == BCH_MEMBER_STATE_evacuating;
}

/*
 * returns true if p1 is better than p2:
 */
static inline bool ptr_better(struct bch_fs *c,
			      const struct extent_ptr_decoded p1,
			      u64 p1_latency,
			      const struct extent_ptr_decoded p2,
			      u64 p2_latency,
			      unsigned preferred_dev,
			      enum bch_read_flags flags)
{
	if (static_branch_unlikely(&bch2_force_reconstruct_read))
		return p1.do_ec_reconstruct > p2.do_ec_reconstruct;

	if (unlikely(p1.do_ec_reconstruct || p2.do_ec_reconstruct))
		return p1.do_ec_reconstruct < p2.do_ec_reconstruct;

	int delta = (int) p2.crc_retry_nr - (int) p1.crc_retry_nr;
	if (unlikely(delta))
		return delta > 0;

#ifdef CONFIG_BCACHEFS_DEBUG
	if (bch2_force_read_device >= 0) {
		delta = (p1.ptr.dev == bch2_force_read_device) -
			(p2.ptr.dev == bch2_force_read_device);
		if (delta)
			return delta > 0;
	}
#endif

	/* Prefer extents with checksums */
	delta = (int) !!(p1.crc.csum_type) -
		(int) !!(p2.crc.csum_type);
	if (unlikely(delta))
		return delta > 0;

	if (unlikely(flags & BCH_READ_soft_require_read_device)) {
		if (p1.ptr.dev == preferred_dev) {
			if (bch2_dev_rotational(c, p2.ptr.dev))
				return true;
			p1_latency /= 256;
		}
		if (p2.ptr.dev == preferred_dev) {
			if (bch2_dev_rotational(c, p1.ptr.dev))
				return false;
			p2_latency /= 256;
		}
	}

	/* Pick at random, biased in favor of the faster device: */

	return bch2_get_random_u64_below(p1_latency + p2_latency) > p1_latency;
}

/*
 * This picks a non-stale pointer, preferably from a device other than @avoid.
 * Avoid can be NULL, meaning pick any. If there are no non-stale pointers to
 * other devices, it will still pick a pointer from avoid.
 */
int bch2_bkey_pick_read_device(struct bch_fs *c, struct bkey_s_c k,
			       struct bch_io_failures *failed,
			       struct extent_ptr_decoded *pick,
			       unsigned preferred_dev,
			       enum bch_read_flags flags)
{
	bool have_csum_errors = false, have_io_errors = false, have_missing_devs = false;
	bool have_dirty_ptrs = false, have_pick = false;

	if (k.k->type == KEY_TYPE_error)
		return bch_err_throw(c, key_type_error);

	rcu_read_lock();
	struct bkey_ptrs_c ptrs = bch2_bkey_ptrs_c(k);
	const union bch_extent_entry *entry;
	struct extent_ptr_decoded p;
	u64 pick_latency;

	bkey_for_each_ptr_decode(k.k, ptrs, p, entry) {
		have_dirty_ptrs |= !p.ptr.cached;

		/*
		 * Unwritten extent: no need to actually read, treat it as a
		 * hole and return 0s:
		 */
		if (p.ptr.unwritten) {
			rcu_read_unlock();
			return 0;
		}

		/* Are we being asked to read from a specific device? */
		if ((flags & BCH_READ_hard_require_read_device) &&
		    p.ptr.dev != preferred_dev)
			continue;

		struct bch_dev *ca = bch2_dev_rcu_noerror(c, p.ptr.dev);

		if (unlikely(!ca && p.ptr.dev != BCH_SB_MEMBER_INVALID)) {
			rcu_read_unlock();
			try(bch2_dev_missing_bkey(c, k, p.ptr.dev));
			rcu_read_lock();
		}

		if (p.ptr.cached && (!ca || dev_ptr_stale_rcu(ca, &p.ptr)))
			continue;

		struct bch_dev_io_failures *f =
			unlikely(failed) ? bch2_dev_io_failures(failed, p.ptr.dev) : NULL;
		if (unlikely(f)) {
			p.crc_retry_nr	   = f->csum_nr;
			p.has_ec	  &= !f->ec_errcode;

			if (ca) {
				have_io_errors	|= f->errcode != 0;
				have_io_errors	|= f->ec_errcode;
			}
			have_csum_errors	|= f->csum_nr != 0;

			if (p.has_ec && (f->errcode || f->csum_nr))
				p.do_ec_reconstruct = true;
			else if (f->errcode ||
				 f->csum_nr > c->opts.checksum_err_retry_nr)
				continue;
		}

		have_missing_devs |= ca && !bch2_dev_is_online(ca);

		if (!ca || !bch2_dev_is_online(ca)) {
			if (!p.has_ec)
				continue;
			p.do_ec_reconstruct = true;
		}

		if (static_branch_unlikely(&bch2_force_reconstruct_read) && p.has_ec)
			p.do_ec_reconstruct = true;

		u64 p_latency = dev_latency(ca);
		/*
		 * Square the latencies, to bias more in favor of the faster
		 * device - we never want to stop issuing reads to the slower
		 * device altogether, so that we can update our latency numbers:
		 */
		p_latency *= p_latency;

		if (!have_pick ||
		    ptr_better(c,
			       p, p_latency,
			       *pick, pick_latency,
			       preferred_dev, flags)) {
			*pick = p;
			pick_latency = p_latency;
			have_pick = true;
		}
	}
	rcu_read_unlock();

	if (have_pick)
		return 1;
	if (!have_dirty_ptrs && !bkey_is_btree_ptr(k.k))
		return 0;
	if (have_missing_devs || !have_dirty_ptrs)
		return bch_err_throw(c, no_device_to_read_from);
	if (have_csum_errors)
		return bch_err_throw(c, data_read_csum_err);
	if (have_io_errors)
		return bch_err_throw(c, data_read_io_err);

	/*
	 * If we get here, we have pointers (bkey_ptrs_validate() ensures that),
	 * but they don't point to valid devices:
	 */
	return bch_err_throw(c, no_devices_valid);
}

/* Extent checksum entries: */

/* returns true if not equal */
static inline bool bch2_crc_unpacked_cmp(struct bch_extent_crc_unpacked l,
					 struct bch_extent_crc_unpacked r)
{
	return (l.csum_type		!= r.csum_type ||
		l.compression_type	!= r.compression_type ||
		l.compressed_size	!= r.compressed_size ||
		l.uncompressed_size	!= r.uncompressed_size ||
		l.offset		!= r.offset ||
		l.live_size		!= r.live_size ||
		l.nonce			!= r.nonce ||
		bch2_crc_cmp(l.csum, r.csum));
}

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

int bch2_bkey_extent_flags_set(struct bch_fs *c, struct bkey_i *k, u64 flags)
{
	try(bch2_request_incompat_feature(c, bcachefs_metadata_version_extent_flags));

	struct bkey_ptrs ptrs = bch2_bkey_ptrs(bkey_i_to_s(k));

	if (ptrs.start != ptrs.end &&
	    extent_entry_type(ptrs.start) == BCH_EXTENT_ENTRY_flags) {
		ptrs.start->flags.flags = flags;
	} else {
		struct bch_extent_flags f = {
			.type	= BIT(BCH_EXTENT_ENTRY_flags),
			.flags	= flags,
		};
		__extent_entry_insert(c, k, ptrs.start, (union bch_extent_entry *) &f);
	}

	return 0;
}
