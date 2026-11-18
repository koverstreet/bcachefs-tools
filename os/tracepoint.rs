// SPDX-License-Identifier: GPL-2.0

//! Tracepoints defined in Rust, and their events: what TRACE_EVENT() makes.
//! An EventClass is DECLARE_EVENT_CLASS(): its record and how it prints;
//! event_class!() makes what the kernel's tracing reads of one, and
//! tracepoint!() a tracepoint of it, with its event. A tracepoint's probes
//! are called with two C strings, TP_PROTO(const char *, const char *):
//! another prototype is another set of probes.

use super::TraceSeq;
use core::ffi::CStr;

/// A TP_STRUCT__entry() field, as the event's format file has it. The kernel
/// lays the fields out from these, each aligned after the last: the record's
/// layout has to match.
pub struct Field {
    pub ty:    &'static CStr,
    pub name:  &'static CStr,
    pub size:  usize,
    pub align: usize,
    /// an array's length, or 0
    pub len:   usize,
}

/// A class of events: the record its probes write to the ring buffer, past
/// the kernel's struct trace_entry, and how it prints.
pub trait EventClass: 'static {
    /// TRACE_SYSTEM
    const SYSTEM: &'static CStr;
    const FIELDS: &'static [Field];
    /// TP_printk(), as the event's format file shows it.
    const PRINT_FMT: &'static CStr;

    /// How many bytes of record @a and @b take.
    fn size(a: &CStr, b: &CStr) -> usize;

    /// TP_fast_assign(): @a and @b into @rec, zeroed, size() long - at @at in
    /// the entry, which a __data_loc's offset counts from.
    fn assign(rec: &mut [u8], at: usize, a: &CStr, b: &CStr);

    /// TP_printk(): @rec, at @at in its entry, into @out.
    fn print(rec: &[u8], at: usize, out: &mut TraceSeq<'_>);
}

/// Where tracepoints aren't - userspace, or a kernel without
/// CONFIG_TRACEPOINTS: none is ever on.
#[cfg(not(all(kernel, CONFIG_TRACEPOINTS)))]
pub mod none {
    use super::EventClass;
    use core::convert::Infallible;
    use core::ffi::CStr;
    use core::marker::PhantomData;

    pub struct Tracepoint<C>(PhantomData<fn() -> C>);

    impl<C: EventClass> Tracepoint<C> {
        pub const fn new(_name: &'static CStr) -> Self {
            Self(PhantomData)
        }

        pub fn call(&self, _a: &CStr, _b: &CStr) {}
    }

    /// What a class prints to: never made, with no tracepoints to print.
    pub struct TraceSeq<'a>(PhantomData<&'a mut ()>, Infallible);

    impl TraceSeq<'_> {
        pub fn write(&mut self, _bytes: &[u8]) {
            match self.1 {}
        }
    }

    #[macro_export]
    macro_rules! tracepoint_enabled {
        ($tp:path) => {{
            let _: &'static $crate::os::tracepoint::Tracepoint<_> = &$tp;
            false
        }};
    }

    #[macro_export]
    macro_rules! event_class {
        ($class:ident) => {};
    }

    #[macro_export]
    macro_rules! tracepoint {
        ($vis:vis $class:ident, $name:ident) => { ::paste::paste! {
            #[allow(non_upper_case_globals)]
            $vis static [<__tracepoint_ $name>]: $crate::os::tracepoint::Tracepoint<$class> =
                $crate::os::tracepoint::Tracepoint::new(
                    $crate::util::ffi::c_str(concat!(stringify!($name), "\0")));
        } };
    }
}
