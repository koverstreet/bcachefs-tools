/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_LOGGED_OPS_FORMAT_H
#define _BCACHEFS_LOGGED_OPS_FORMAT_H

enum logged_ops_inums {
	LOGGED_OPS_INUM_logged_ops,
	LOGGED_OPS_INUM_inode_cursors,
};

struct bch_logged_op_truncate {
	struct bch_val		v;
	__le32			subvol;
	__le32			pad;
	__le64			inum;
	__le64			new_i_size;
};

enum logged_op_finsert_state {
	LOGGED_OP_FINSERT_start,
	LOGGED_OP_FINSERT_shift_extents,
	LOGGED_OP_FINSERT_finish,
};

struct bch_logged_op_finsert {
	struct bch_val		v;
	__u8			state;
	__u8			pad[3];
	__le32			subvol;
	__le64			inum;
	__le64			dst_offset;
	__le64			src_offset;
	__le64			pos;
};

/*
 * Push one inode version's io options up to its ancestor snapshot versions, so
 * that data written before this branch existed sees them - see
 * bch2_inode_opt_propagate().
 *
 * The two snapshot ids have to be separate fields: "is this version off the
 * path we are propagating along" is asked relative to the origin, so folding
 * them together makes the origin itself look like a sibling one level up.
 *
 * No value or option id: the inode key at (@inum, @origin_snapshot) is the
 * source of truth, so a resumed op recomputes rather than replaying a value
 * that may have changed since.
 */
struct bch_logged_op_inode_opt_propagate {
	struct bch_val		v;
	__le64			inum;
	__le32			origin_snapshot;
	__le32			cursor_snapshot;
};

struct bch_logged_op_stripe_update {
	struct bch_val		v;
	__le64			old_idx;
	__le64			new_idx;
	__u8			old_blocks_nr;
	__u8			old_block_map[16];
	__u8			pad[7];
};

#endif /* _BCACHEFS_LOGGED_OPS_FORMAT_H */
