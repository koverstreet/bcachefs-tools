// SPDX-License-Identifier: GPL-2.0

/*
 * Bindgen shims for the Rust memalloc guards (util/locking.rs).
 * memalloc_flags_save/restore are kernel static inlines outside bcachefs, and
 * PF_MEMALLOC_NOIO is a bare #define that doesn't reach Rust; wrap them under
 * bcachefs-owned rust_* names so the flag stays on the C side. Save is
 * per-flag; restore just replays saved flags.
 *
 * These are real (out-of-line) functions, not static inlines: both the fs and
 * bch_bindgen bindgen passes see util/locking.h, and a static inline would
 * have each emit its own wrap_static_fns wrapper for the same symbol - a
 * duplicate at link. A plain declaration binds to one shared definition.
 */

#include "bcachefs.h"
#include "util/locking.h"

unsigned int rust_memalloc_noio_save(void)
{
	return memalloc_flags_save(PF_MEMALLOC_NOIO);
}

void rust_memalloc_flags_restore(unsigned int flags)
{
	memalloc_flags_restore(flags);
}
