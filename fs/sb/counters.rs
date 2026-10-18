// SPDX-License-Identifier: GPL-2.0

//! Events - sb/counters.h's event_inc(), event_add() and event_trace() - for
//! Rust, each event a type; C's macros are as they were.
//!
//! An event is one of BCH_PERSISTENT_COUNTERS() (counters_format.rs): a
//! counter in the filesystem's counters.now[], per CPU, and the tracepoint of
//! the same name every counter has (debug/trace.h). Its type, events::<name>,
//! carries what the counter counts - occurrences (TYPE_COUNTER) or sectors
//! (TYPE_SECTORS) - so event_inc() takes the one and event_add() the other,
//! as C's counter_typecheck() checks.
//!
//! Counters are added to with util/percpu.rs's this_cpu_add(). A tracepoint is
//! checked as C's trace_<name>_enabled() is, a static branch on its key -
//! kernel::static_branch_unlikely!() - so a disabled one costs a NOP; its
//! message is built only once it's on, out of line, as __event_trace() builds
//! it, and handed to C's rust_do_trace_<name>(), which debug/trace.c makes for
//! every tracepoint with CREATE_RUST_TRACE_POINTS. Userspace has no
//! tracepoints: none is ever on.
//!
//! XXX: a tracepoint fired only from Rust gets no __tracepoint_check entry, as
//! C's use sites leave, so a WARN_ON_UNUSED_TRACEPOINTS build calls it unused.

use crate::c;
use crate::fs::Fs;
use crate::util::percpu::this_cpu_add;
use crate::util::Printbuf;

/// What an event's counter counts.
pub trait Kind {}

/// Occurrences: TYPE_COUNTER, counted with event_inc().
pub enum Count {}

/// An amount in sectors: TYPE_SECTORS, added with event_add().
pub enum Sectors {}

impl Kind for Count {}
impl Kind for Sectors {}

/// One of BCH_PERSISTENT_COUNTERS(): its counter, and its tracepoint.
pub trait Event: Copy {
    type Kind: Kind;

    /// Its counter's index in counters.now[]: BCH_COUNTER_<name>.
    const IDX: usize;

    /// Whether its tracepoint is on: trace_<name>_enabled().
    fn trace_enabled() -> bool;

    /// Fire its tracepoint, with @msg: trace_<name>().
    fn trace(fs: &Fs, msg: &Printbuf);
}

macro_rules! event_kind {
    (TYPE_COUNTER) => { Count };
    (TYPE_SECTORS) => { Sectors };
}

macro_rules! events {
    ($(($name:ident, $id:tt, $ty:tt $(, $($rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        /// The events of BCH_PERSISTENT_COUNTERS(), each a type.
        #[allow(non_camel_case_types)]
        pub mod events {
            $(
                #[derive(Clone, Copy)]
                pub struct $name;
            )*
        }

        $(
            impl Event for events::$name {
                type Kind = event_kind!($ty);

                const IDX: usize = c::[<BCH_COUNTER_ $name>] as usize;

                #[cfg(all(kernel, CONFIG_TRACEPOINTS))]
                #[inline(always)]
                fn trace_enabled() -> bool {
                    // SAFETY: a tracepoint's key, which C defines
                    unsafe {
                        ::kernel::static_branch_unlikely!(tracepoints::[<__tracepoint_ $name>],
                                                          c::tracepoint, key)
                    }
                }

                #[cfg(not(all(kernel, CONFIG_TRACEPOINTS)))]
                #[inline(always)]
                fn trace_enabled() -> bool {
                    false
                }

                #[cfg(all(kernel, CONFIG_TRACEPOINTS))]
                fn trace(fs: &Fs, msg: &Printbuf) {
                    // SAFETY: @fs is live, and @msg a C string the tracepoint
                    // copies
                    unsafe {
                        tracepoints::[<rust_do_trace_ $name>](fs.as_ptr(), msg.as_c_str().as_ptr());
                    }
                }

                #[cfg(not(all(kernel, CONFIG_TRACEPOINTS)))]
                fn trace(_fs: &Fs, _msg: &Printbuf) {}
            }
        )*

        /// The tracepoints, C's: debug/trace.c's.
        #[cfg(all(kernel, CONFIG_TRACEPOINTS))]
        mod tracepoints {
            use crate::c;

            extern "C" {
                $(
                    pub static [<__tracepoint_ $name>]: c::tracepoint;
                    pub fn [<rust_do_trace_ $name>](c: *mut c::bch_fs, str: *const ::core::ffi::c_char);
                )*
            }
        }
    } };
}
c::BCH_PERSISTENT_COUNTERS!(events);

impl Fs {
    /// Count @_e once: event_inc().
    #[inline(always)]
    pub fn event_inc<E: Event<Kind = Count>>(&self, _e: E) {
        self.event_count::<E>(1);
    }

    /// Add @nr sectors to @_e: event_add().
    #[inline(always)]
    pub fn event_add<E: Event<Kind = Sectors>>(&self, _e: E, nr: u64) {
        self.event_count::<E>(nr);
    }

    #[inline(always)]
    fn event_count<E: Event>(&self, nr: u64) {
        // SAFETY: counters.now is per CPU, BCH_COUNTER_NR of them, from
        // bch2_fs_counters_init() until the filesystem is freed
        unsafe { this_cpu_add(self.counters.now.wrapping_add(E::IDX), nr) };
    }

    /// Fire @e's tracepoint, if it's on, with the message @msg writes - its
    /// lines after the first indented by 2: event_trace().
    #[inline(always)]
    pub fn event_trace<E: Event>(&self, e: E, msg: impl FnOnce(&mut Printbuf)) {
        if E::trace_enabled() {
            self.event_trace_on(e, msg);
        }
    }

    #[cold]
    #[inline(never)]
    fn event_trace_on<E: Event>(&self, _e: E, msg: impl FnOnce(&mut Printbuf)) {
        let mut buf = Printbuf::new();
        buf.indent_add_nextline(2);
        msg(&mut buf);
        E::trace(self, &buf);
    }

    /// event_trace(), then event_inc(): event_inc_trace().
    #[inline(always)]
    pub fn event_inc_trace<E: Event<Kind = Count>>(&self, e: E, msg: impl FnOnce(&mut Printbuf)) {
        self.event_trace(e, msg);
        self.event_inc(e);
    }

    /// event_trace(), then event_add(): event_add_trace().
    #[inline(always)]
    pub fn event_add_trace<E: Event<Kind = Sectors>>(&self, e: E, nr: u64,
                                                    msg: impl FnOnce(&mut Printbuf)) {
        self.event_trace(e, msg);
        self.event_add(e, nr);
    }
}
