// SPDX-License-Identifier: GPL-2.0

//! Tracepoints - sb/counters.h's event_trace() - for Rust, each a type,
//! events::<name>: every counter's (BCH_PERSISTENT_COUNTERS(), sb/counters.rs),
//! and those with no counter, BCH_NOCOUNTER_TRACEPOINTS() and
//! BCH_PATH_TRACEPOINTS(). A path tracepoint exists only
//! with CONFIG_BCACHEFS_PATH_TRACEPOINTS, and is never on without it.
//!
//! A tracepoint is checked as C's trace_<name>_enabled() is, a static branch on
//! its key - os::jump_label's static_branch_unlikely!() - so a disabled one
//! costs a NOP; its message is built only once it's on, out of line, as
//! __event_trace() builds it, and handed to its probes. The tracepoints and
//! their events' class are trace/tracepoints.rs's, made with os::tracepoint -
//! which in userspace, or without CONFIG_TRACEPOINTS, has none: none is ever
//! on.

use crate::c;
use crate::fs::Fs;
use crate::util::Printbuf;
use cstruct_macros::xmacro;

mod tracepoints;

/* The tracepoints that have no counter: */
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

/// A tracepoint - one of BCH_PERSISTENT_COUNTERS()'s, or of those that have
/// no counter, BCH_NOCOUNTER_TRACEPOINTS() and BCH_PATH_TRACEPOINTS(). One
/// that doesn't exist - a path tracepoint without
/// CONFIG_BCACHEFS_PATH_TRACEPOINTS - is never on.
pub trait TraceEvent: Copy {
    /// Whether its tracepoint is on: trace_<name>_enabled().
    #[inline(always)]
    fn trace_enabled() -> bool {
        false
    }

    /// Fire its tracepoint, with @msg: trace_<name>().
    fn trace(_fs: &Fs, _msg: &Printbuf) {}
}

/// Each event a type: both lists' entries start with the name.
macro_rules! event_types {
    ($(($name:ident $(, $($rest:tt)*)?)),* $(,)?) => {
        $(
            #[derive(Clone, Copy)]
            pub struct $name;
        )*
    };
}

/// The events, each a type: events::<name>.
#[allow(non_camel_case_types)]
pub mod events {
    use crate::c;
    c::PersistentCounter_arms!(event_types);
    super::BCH_NOCOUNTER_TRACEPOINTS!(event_types);
    super::BCH_PATH_TRACEPOINTS!(event_types);
}

/// Their tracepoints.
macro_rules! trace_events {
    ($(($name:ident $(, $($rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        $(
            impl TraceEvent for events::$name {
                #[inline(always)]
                fn trace_enabled() -> bool {
                    crate::tracepoint_enabled!(tracepoints::[<__tracepoint_ $name>])
                }

                fn trace(fs: &Fs, msg: &Printbuf) {
                    tracepoints::[<__tracepoint_ $name>].call(fs.name(), msg.as_c_str());
                }
            }
        )*
    } };
}

macro_rules! no_trace_events {
    ($(($name:ident $(, $($rest:tt)*)?)),* $(,)?) => {
        $( impl TraceEvent for events::$name {} )*
    };
}
c::PersistentCounter_arms!(trace_events);
BCH_NOCOUNTER_TRACEPOINTS!(trace_events);
#[cfg(CONFIG_BCACHEFS_PATH_TRACEPOINTS)]
BCH_PATH_TRACEPOINTS!(trace_events);
#[cfg(not(CONFIG_BCACHEFS_PATH_TRACEPOINTS))]
BCH_PATH_TRACEPOINTS!(no_trace_events);

impl Fs {
    /// Fire @e's tracepoint, if it's on, with the message @msg writes - its
    /// lines after the first indented by 2: event_trace().
    #[inline(always)]
    pub fn event_trace<E: TraceEvent>(&self, e: E, msg: impl FnOnce(&mut Printbuf)) {
        if E::trace_enabled() {
            self.event_trace_on(e, msg);
        }
    }

    #[cold]
    #[inline(never)]
    fn event_trace_on<E: TraceEvent>(&self, _e: E, msg: impl FnOnce(&mut Printbuf)) {
        let mut buf = Printbuf::new();
        buf.indent_add_nextline(2);
        msg(&mut buf);
        E::trace(self, &buf);
    }
}
