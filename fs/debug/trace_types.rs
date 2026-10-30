// SPDX-License-Identifier: GPL-2.0

//! The tracepoints that have no counter: debug/trace.h's DEFINE_EVENT()s
//! besides BCH_PERSISTENT_COUNTERS()'s, and sb/counters.rs's trace-only
//! events.

use cstruct_macros::c_xmacro;

c_xmacro! {
    BCH_NOCOUNTER_TRACEPOINTS(x) {
        (accounting_mem_insert),
        (journal_entry_close),
        (extent_trim_atomic),
        (path_downgrade),
        (btree_iter_peek_slot),
        (__btree_iter_peek),
        (btree_iter_peek_max),
        (btree_iter_peek_prev_min),
    }
}
