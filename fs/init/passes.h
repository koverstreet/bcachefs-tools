#ifndef _BCACHEFS_RECOVERY_PASSES_H
#define _BCACHEFS_RECOVERY_PASSES_H

#include "init/passes_gen.h"

static inline bool recovery_pass_will_run(struct bch_fs *c, enum bch_recovery_pass pass)
{
	return unlikely(test_bit(BCH_FS_in_recovery, &c->flags) &&
			c->recovery.current_passes & BIT_ULL(pass));
}

#endif /* _BCACHEFS_RECOVERY_PASSES_H */
