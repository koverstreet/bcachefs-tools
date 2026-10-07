/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_SB_COUNTERS_FORMAT_INLINE_H
#define _BCACHEFS_SB_COUNTERS_FORMAT_INLINE_H

/* Included at the end of sb/counters_format.h. */

__maybe_unused
static const enum bch_counters_flags bch2_counter_flags[] = {
#define x(t, n, flags, ...) [BCH_COUNTER_##t] = flags,
	BCH_PERSISTENT_COUNTERS()
#undef x
};

static inline void __maybe_unused check_bch_counter_ids_unique(void) {
	switch(0){
#define x(t, n, ...) case (n):
        BCH_PERSISTENT_COUNTERS();
#undef x
		;
	}
}

#endif /* _BCACHEFS_SB_COUNTERS_FORMAT_INLINE_H */
