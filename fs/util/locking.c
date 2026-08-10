// SPDX-License-Identifier: GPL-2.0

/*
 * Out-of-line homes for the util/locking.h Rust shims. See the header for why
 * these can't be static inlines (duplicate wrap_static_fns wrappers across the
 * two bindgen passes).
 */

#include "bcachefs.h"
#include "util/locking.h"
#include "util/printbuf.h"

/*
 * Who is holding this, since when, and from where - see the owner tracking note
 * in locking.h. Read without the lock: safe (no pointers are stored, only a pid
 * and a text address) but racy, so a holder that has just left can still be
 * named.
 */
void bch2_mutex_noio_to_text(struct printbuf *out, struct mutex_noio *m)
{
	unsigned long held_from = smp_load_acquire(&m->held_from);

	if (!held_from) {
		prt_str(out, "not held");
		return;
	}

	prt_printf(out, "held %ums by pid %u at %pS",
		   jiffies_to_msecs(jiffies - READ_ONCE(m->held_at)),
		   READ_ONCE(m->held_by),
		   (void *) held_from);
}

unsigned int rust_memalloc_noio_save(void)
{
	return memalloc_flags_save(PF_MEMALLOC_NOIO);
}

void rust_memalloc_flags_restore(unsigned int flags)
{
	memalloc_flags_restore(flags);
}
