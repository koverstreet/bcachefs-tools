// SPDX-License-Identifier: GPL-2.0

//! Tracepoints - sb/counters.h's event_trace() - for Rust, each a type,
//! events::<name>: every counter's (BCH_PERSISTENT_COUNTERS(), sb/counters.rs),
//! and those with no counter, BCH_NOCOUNTER_TRACEPOINTS() and
//! BCH_PATH_TRACEPOINTS() (debug/trace_types.rs). A path tracepoint exists only
//! with CONFIG_BCACHEFS_PATH_TRACEPOINTS, and is never on without it.
//!
//! A tracepoint is checked as C's trace_<name>_enabled() is, a static branch on
//! its key - util/jump_label.rs's static_branch_unlikely!() - so a disabled one
//! costs a NOP; its message is built only once it's on, out of line, as
//! __event_trace() builds it, and handed to its probes. The tracepoints and
//! their events are trace/tracepoints.rs's. Userspace has no tracepoints: none
//! is ever on.

use crate::c;
use crate::fs::Fs;
use crate::util::Printbuf;

#[cfg(all(kernel, CONFIG_TRACEPOINTS))]
mod tracepoints;

/// A tracepoint - one of BCH_PERSISTENT_COUNTERS()'s, or of those that have
/// no counter, BCH_NOCOUNTER_TRACEPOINTS() and BCH_PATH_TRACEPOINTS().
pub trait TraceEvent: Copy {
    /// Whether its tracepoint is on: trace_<name>_enabled().
    fn trace_enabled() -> bool;

    /// Fire its tracepoint, with @msg: trace_<name>().
    fn trace(fs: &Fs, msg: &Printbuf);
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
    c::bch_persistent_counters_arms!(event_types);
    c::BCH_NOCOUNTER_TRACEPOINTS!(event_types);
    c::BCH_PATH_TRACEPOINTS!(event_types);
}

/// Their tracepoints, real where [on] holds - by default, a kernel build with
/// tracepoints.
macro_rules! trace_events {
    ($(($name:ident $(, $($rest:tt)*)?)),* $(,)?) => {
        trace_events! { [all(kernel, CONFIG_TRACEPOINTS)] $(($name)),* }
    };
    ([$on:meta] $(($name:ident)),*) => { ::paste::paste! {
        $(
            impl TraceEvent for events::$name {
                #[cfg($on)]
                #[inline(always)]
                fn trace_enabled() -> bool {
                    unsafe {
                        crate::static_branch_unlikely!(tracepoints::[<__tracepoint_ $name>],
                                                       c::tracepoint, key)
                    }
                }

                #[cfg(not($on))]
                #[inline(always)]
                fn trace_enabled() -> bool {
                    false
                }

                #[cfg($on)]
                fn trace(fs: &Fs, msg: &Printbuf) {
                    tracepoints::call(&tracepoints::[<__tracepoint_ $name>], fs.name(), msg.as_c_str());
                }

                #[cfg(not($on))]
                fn trace(_fs: &Fs, _msg: &Printbuf) {}
            }
        )*
    } };
}
c::bch_persistent_counters_arms!(trace_events);
c::BCH_NOCOUNTER_TRACEPOINTS!(trace_events);
c::BCH_PATH_TRACEPOINTS!(trace_events
    [all(kernel, CONFIG_TRACEPOINTS, CONFIG_BCACHEFS_PATH_TRACEPOINTS)]);

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
