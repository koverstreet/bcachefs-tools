// SPDX-License-Identifier: GPL-2.0

/*
 * Out-of-line homes for the util/locking.h Rust shims. See the header for why
 * these can't be static inlines (duplicate wrap_static_fns wrappers across the
 * two bindgen passes).
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
