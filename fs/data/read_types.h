/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DATA_READ_TYPES_H
#define _BCACHEFS_DATA_READ_TYPES_H

struct bch_read_err_report {
	struct mutex		lock;
	u32			errors;
	struct printbuf		msg;
};

struct bch_read_bio {
	struct bch_fs		*c;
	struct bch_dev		*ca;	/* stashed at submit; see bch_write_bio */
	u64			start_time;
	u64			submit_time;

	/*
	 * Reads will often have to be split, and if the extent being read from
	 * was checksummed or compressed we'll also have to allocate bounce
	 * buffers and copy the data back into the original bio.
	 *
	 * If we didn't have to split, we have to save and restore the original
	 * bi_end_io - @split below indicates which:
	 */
	union {
	struct bch_read_bio	*parent;
	bio_end_io_t		*end_io;
	};

	/*
	 * Saved copy of bio->bi_iter, from submission time - allows us to
	 * resubmit on IO error, and also to copy data back to the original bio
	 * when we're bouncing:
	 */
	struct bvec_iter	bvec_iter;

	unsigned		offset_into_extent;

	u16			flags;
	union {
	struct {
	u16			data_update:1,
				data_update_verify_decompress:1,
				promote:1,
				bounce:1,
				split:1,
				narrow_crcs:1,
				saw_error:1,
				self_healing:1,
				context:2;
	};
	u16			_state;
	};
	s16			ret;
#ifdef CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS
	unsigned		list_idx;
#endif

	struct extent_ptr_decoded pick;

	/*
	 * pos we read from - different from data_pos for indirect extents:
	 */
	u32			subvol;
	struct bpos		read_pos;

	/*
	 * start pos of data we read (may not be pos of data we want) - for
	 * promote, narrow extents paths:
	 */
	enum btree_id		data_btree;
	struct bpos		data_pos;
	struct bversion		version;

	struct bch_inode_opts	opts;

	struct bch_io_failures	*failed;
	struct bch_read_err_report *err_report;

	struct work_struct	work;

	struct bio		bio;
};

struct bch_devs_mask;

struct cache_promote_op;

struct extent_ptr_decoded;

struct promote_op;

#endif /* _BCACHEFS_DATA_READ_TYPES_H */
