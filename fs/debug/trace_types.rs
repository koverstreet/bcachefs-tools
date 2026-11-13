// SPDX-License-Identifier: GPL-2.0

//! The tracepoints that have no counter: debug/trace.h's DEFINE_EVENT()s
//! besides BCH_PERSISTENT_COUNTERS()'s, and debug/trace.rs's trace-only
//! events.

use cstruct_macros::xmacro;

xmacro! {
    BCH_NOCOUNTER_TRACEPOINTS {
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

xmacro! {
    BCH_PATH_TRACEPOINTS {
        (update_by_path),
        (btree_path_traverse_start),
        (btree_path_traverse_end),
        (btree_path_set_pos),
        (btree_path_lock),
        (btree_path_should_be_locked),
        (btree_path_clone),
        (btree_path_get_ll),
        (btree_path_put_ll),
        (btree_path_get),
        (btree_path_alloc),
        (btree_path_free),
    }
}
