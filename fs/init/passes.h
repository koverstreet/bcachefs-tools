#ifndef _BCACHEFS_RECOVERY_PASSES_H
#define _BCACHEFS_RECOVERY_PASSES_H

#include "init/passes_gen.h"
#include "init/passes_defs_gen.h"

/*
 * A pass is only ever scheduled because it has work to do, so anything
 * scheduled means we need to be able to write. bch2_snapshots_read() arms
 * delete_dead_interior_snapshots when it finds empty interior nodes, and it
 * runs before us precisely so that we can see it here - that pass has no way
 * to ask for writes later, it just fails with erofs_trans_commit.
 *
 * Both scheduled sets: which one a pass lands in is is_persistent() in
 * passes.rs, which says nothing about whether it writes. c->opts.recovery_passes
 * stays because bch2_fs_start() calls us before recovery seeds the ephemeral set.
 */
static inline bool go_rw_in_recovery(struct bch_fs *c)
{
	return test_bit(BCH_FS_may_upgrade_downgrade, &c->flags) &&
		(c->journal_keys.nr ||
		!c->opts.read_only ||
		!c->sb.clean ||
		c->opts.recovery_passes ||
		c->sb.recovery_passes_required ||
		c->recovery.scheduled_passes_ephemeral ||
		(c->opts.fsck && !(c->sb.features & BIT_ULL(BCH_FEATURE_no_alloc_info))));
}

static inline bool recovery_pass_will_run(struct bch_fs *c, enum bch_recovery_pass pass)
{
	return unlikely(test_bit(BCH_FS_in_recovery, &c->flags) &&
			c->recovery.current_passes & BIT_ULL(pass));
}

#endif /* _BCACHEFS_RECOVERY_PASSES_H */
