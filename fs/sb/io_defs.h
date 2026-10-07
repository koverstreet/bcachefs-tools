/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_SB_IO_DEFS_H
#define _BCACHEFS_SB_IO_DEFS_H

#include "enum_kind.h"

struct bch_sb_field_ops {
	int	(*validate)(struct bch_sb *, struct bch_sb_field *,
			    enum bch_validate_flags, struct printbuf *);
	void	(*to_text)(struct printbuf *, struct bch_fs *, struct bch_sb *, struct bch_sb_field *);
};

/*
 * bringup: this write is part of bringing the filesystem up - allowed before
 * a start has begun, see __bch2_write_super()
 */
enum __enum_flags bch_sb_write_flags {
	BCH_SB_WRITE_bringup	= BIT(0),
};

/*
 * Permission to modify a superblock field, and the thing that writes it back.
 * Hold sb_lock - guard(mutex_noio)(&c->sb_lock) - and declare one of these
 * under it. Declaration order is load-bearing: declared after the lock guard,
 * this destructs first, so the write happens while sb_lock is still held.
 *
 * The setters return whether the fact was NEW, which is the caller's business
 * (an fsck message unsuppresses on novelty). The write-back accumulates
 * separately, so forgetting to use that return can't lose a superblock write.
 */
struct sb_write {
	struct bch_fs	*c;
	bool		dirty;
	bool		replicas;
	enum bch_sb_write_flags flags;
};

#endif /* _BCACHEFS_SB_IO_DEFS_H */
