// SPDX-License-Identifier: GPL-2.0

//! The module loader finds a tracepoint through its sections, as C's: its
//! tracepoint in __tracepoints_ptrs, its event in _ftrace_events, and its raw
//! BPF tracepoint in __bpf_raw_tp_map. A tracepoint has no static call - its
//! static_call_key NULL, which the kernel takes as it does a synthetic
//! event's - so call() walks its probes as __traceiter_<name>() does.

#[path = "../tracepoint.rs"]
mod common;
pub use common::*;

#[cfg(not(CONFIG_TRACEPOINTS))]
pub use common::none::*;

#[cfg(CONFIG_TRACEPOINTS)]
pub use on::*;

#[cfg(CONFIG_TRACEPOINTS)]
mod on {
    use super::{EventClass, Field};
    use core::cell::UnsafeCell;
    use core::ffi::CStr;
    use core::marker::PhantomData;
    use core::mem::{size_of, transmute, MaybeUninit};
    use core::{ptr, slice};

    use crate::c;
    use crate::util::ffi::{c_char, c_int, c_ulong, c_void};

    /// A static the kernel's tracing writes, under its own locks.
    #[doc(hidden)]
    #[repr(transparent)]
    pub struct TracingStatic<T>(UnsafeCell<T>);

    unsafe impl<T> Sync for TracingStatic<T> {}

    impl<T> TracingStatic<T> {
        pub const fn new(v: T) -> Self {
            Self(UnsafeCell::new(v))
        }

        pub const fn get(&self) -> *mut T {
            self.0.get()
        }
    }

    /// A tracepoint, of an event of class @C: tracepoint!()'s.
    #[repr(transparent)]
    pub struct Tracepoint<C>(TracingStatic<c::tracepoint>, PhantomData<fn() -> C>);

    // ── The call ─────────────────────────────────────────────────────────

    type Probe = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char);

    /// preempt_disable_notrace() until dropped.
    struct PreemptNotrace;

    impl PreemptNotrace {
        fn disable() -> Self {
            unsafe { c::rust_preempt_disable_notrace() };
            Self
        }
    }

    impl Drop for PreemptNotrace {
        fn drop(&mut self) {
            unsafe { c::rust_preempt_enable_notrace() };
        }
    }

    /// A read-side section of tracepoint_srcu, which a probe is freed a grace
    /// period of after it's unregistered: guard(srcu_fast_notrace)'s.
    #[cfg(linux_ge = "7.0")]
    struct TracepointSrcu(c_ulong);

    #[cfg(linux_ge = "7.0")]
    impl TracepointSrcu {
        fn read_lock() -> Self {
            Self(srcu_fast::lock())
        }
    }

    #[cfg(linux_ge = "7.0")]
    impl Drop for TracepointSrcu {
        fn drop(&mut self) {
            srcu_fast::unlock(self.0)
        }
    }

    /*
     * srcu_read_lock_fast_notrace() and _unlock() as srcutree.h has them, where
     * that's all they are - tree SRCU, no RCU lockdep, NMI-safe this_cpu ops:
     * this CPU's count of the current reader slot's locks, or its unlocks,
     * bumped, a compiler barrier on the section's side
     */
    #[cfg(all(linux_ge = "7.0", CONFIG_TREE_SRCU, not(CONFIG_PROVE_RCU), not(CONFIG_NEED_SRCU_NMI_SAFE)))]
    mod srcu_fast {
        use crate::c;
        use crate::os::percpu::PerCpuRef;
        use crate::util::ffi::c_ulong;
        use core::mem::offset_of;
        use core::sync::atomic::{compiler_fence, Ordering::SeqCst};
        use kernel::bindings::srcu_ctr;

        pub(super) fn lock() -> c_ulong {
            let scp = unsafe { (&raw const c::tracepoint_srcu.srcu_ctrp).read_volatile() };
            bump(scp, offset_of!(srcu_ctr, srcu_locks));
            compiler_fence(SeqCst);
            scp as c_ulong
        }

        pub(super) fn unlock(scp: c_ulong) {
            compiler_fence(SeqCst);
            bump(scp as *mut srcu_ctr, offset_of!(srcu_ctr, srcu_unlocks));
        }

        fn bump(scp: *mut srcu_ctr, count: usize) {
            unsafe { PerCpuRef::<c_ulong>::from_raw(scp.wrapping_byte_add(count).cast()) }.add(1)
        }
    }

    #[cfg(all(linux_ge = "7.0", not(all(CONFIG_TREE_SRCU, not(CONFIG_PROVE_RCU), not(CONFIG_NEED_SRCU_NMI_SAFE)))))]
    mod srcu_fast {
        use crate::c;
        use crate::util::ffi::c_ulong;

        pub(super) fn lock() -> c_ulong {
            unsafe { c::rust_srcu_read_lock_fast_notrace(&raw mut c::tracepoint_srcu) }
        }

        pub(super) fn unlock(scp: c_ulong) {
            unsafe { c::rust_srcu_read_unlock_fast_notrace(&raw mut c::tracepoint_srcu, scp) }
        }
    }

    impl<C: EventClass> Tracepoint<C> {
        pub const fn new(name: &'static CStr) -> Self {
            Self(TracingStatic::new(c::tracepoint { name: name.as_ptr().cast(), ..::pin_init::zeroed() }),
                 PhantomData)
        }

        #[doc(hidden)]
        pub const fn as_ptr(&self) -> *mut c::tracepoint {
            self.0.get()
        }

        /// Call its probes with @a and @b: __do_trace_<name>(). Before 7.0 they
        /// run with preemption off; from 7.0 under tracepoint_srcu, preemptible,
        /// and the event's probes turn it off themselves.
        pub fn call(&self, a: &CStr, b: &CStr) {
            #[cfg(linux_ge = "7.0")]
            let _guard = TracepointSrcu::read_lock();
            #[cfg(not(linux_ge = "7.0"))]
            let _guard = PreemptNotrace::disable();

            let mut it = unsafe { ptr::read_volatile(&raw const (*self.as_ptr()).funcs) };
            if it.is_null() {
                return;
            }
            loop {
                unsafe {
                    let probe = transmute::<*mut c_void, Probe>(ptr::read_volatile(&raw const (*it).func));
                    probe((*it).data, a.as_ptr().cast(), b.as_ptr().cast());
                    it = it.add(1);
                    if (*it).func.is_null() {
                        break;
                    }
                }
            }
        }
    }

    // ── The probes ───────────────────────────────────────────────────────

    /// Where a class's record starts in the entry: after the kernel's.
    const HEADER: usize = size_of::<c::trace_entry>();

    /// The @size bytes of record past @entry's header, zeroed.
    ///
    /// # Safety
    /// @entry has room for its header and them, and nothing else uses them.
    unsafe fn record<'a>(entry: *mut c_void, size: usize) -> &'a mut [u8] {
        unsafe {
            let rec = entry.cast::<u8>().add(HEADER);
            rec.write_bytes(0, size);
            slice::from_raw_parts_mut(rec, size)
        }
    }

    /// The ftrace probe: trace_event_raw_event_<class>().
    unsafe extern "C" fn probe<C: EventClass>(data: *mut c_void, a: *const c_char, b: *const c_char) {
        #[cfg(linux_ge = "7.0")]
        let _preempt = PreemptNotrace::disable();

        let file = data.cast::<c::trace_event_file>();
        if unsafe { c::trace_trigger_soft_disabled(file) } {
            return;
        }

        let (a, b) = unsafe { (CStr::from_ptr(a.cast()), CStr::from_ptr(b.cast())) };
        let size = C::size(a, b);
        let mut fbuffer = MaybeUninit::<c::trace_event_buffer>::uninit();
        let entry = unsafe {
            c::trace_event_buffer_reserve(fbuffer.as_mut_ptr(), file, (HEADER + size) as _)
        };
        if entry.is_null() {
            return;
        }
        unsafe {
            C::assign(record(entry, size), HEADER, a, b);
            c::trace_event_buffer_commit(fbuffer.as_mut_ptr());
        }
    }

    /// The perf probe: perf_trace_<class>().
    #[cfg(CONFIG_PERF_EVENTS)]
    unsafe extern "C" fn perf_probe<C: EventClass>(data: *mut c_void, a: *const c_char, b: *const c_char) {
        #[cfg(linux_ge = "7.0")]
        let _preempt = PreemptNotrace::disable();

        let call = data.cast::<c::trace_event_call>();
        let head = unsafe { crate::os::percpu::bch2_this_cpu_ptr((*call).perf_events.cast()) }
            .cast::<c::hlist_head>();
        if !unsafe { c::bpf_prog_array_valid(call) } &&
            unsafe { ptr::read_volatile(&raw const (*head).first) }.is_null() {
            return;
        }

        let (a, b) = unsafe { (CStr::from_ptr(a.cast()), CStr::from_ptr(b.cast())) };
        let rec_size = C::size(a, b);
        let size = (HEADER + rec_size + size_of::<u32>()).next_multiple_of(size_of::<u64>())
            - size_of::<u32>();
        let mut regs = ptr::null_mut();
        let mut rctx = 0;
        let entry = unsafe { c::perf_trace_buf_alloc(size as i32, &mut regs, &mut rctx) };
        if entry.is_null() {
            return;
        }
        unsafe {
            c::rust_perf_fetch_caller_regs(regs);
            C::assign(record(entry, rec_size), HEADER, a, b);
            c::perf_trace_run_bpf_submit(entry, size as i32, rctx, call, 1, regs, head,
                                         ptr::null_mut());
        }
    }

    /// The raw BPF tracepoint's: __bpf_trace_<class>().
    #[cfg(CONFIG_BPF_EVENTS)]
    unsafe extern "C" fn bpf_trace(data: *mut c_void, a: *const c_char, b: *const c_char) {
        unsafe { c::bpf_trace_run2(data.cast(), a as u64, b as u64) };
    }

    /// What a class prints to: an event's output, iter->seq.
    pub struct TraceSeq<'a>(*mut c::trace_seq, PhantomData<&'a mut c::trace_seq>);

    impl TraceSeq<'_> {
        pub fn write(&mut self, bytes: &[u8]) {
            unsafe { c::trace_seq_putmem(self.0, bytes.as_ptr().cast(), bytes.len() as _) }
        }
    }

    /// The event's output, trace_raw_output_<class>(): TP_printk().
    unsafe extern "C" fn output<C: EventClass>(iter: *mut c::trace_iterator, _flags: c_int,
                                               event: *mut c::trace_event) -> c::print_line_t {
        // an int, and print_line_t an enum, whose type is the bindings': signed
        // or unsigned, by kernel and bindgen
        let ret = unsafe { c::trace_raw_output_prep(iter, event) };
        if ret != c::print_line_t_TRACE_TYPE_HANDLED as i32 {
            return ret as c::print_line_t;
        }
        unsafe {
            let entry = (*iter).ent.cast::<u8>();
            let rec = slice::from_raw_parts(entry.add(HEADER), (*iter).ent_size as usize - HEADER);
            C::print(rec, HEADER, &mut TraceSeq(&raw mut (*iter).seq, PhantomData));
            c::trace_handle_return(&raw mut (*iter).seq)
        }
    }

    // ── What the kernel's tracing reads ──────────────────────────────────

    /// TP_STRUCT__entry()'s. Not signed: the kernel's char is unsigned.
    const fn field(f: &Field) -> c::trace_event_fields {
        let mut out: c::trace_event_fields = ::pin_init::zeroed();
        out.type_ = f.ty.as_ptr().cast();
        out.__bindgen_anon_1.__bindgen_anon_1.name = f.name.as_ptr().cast();
        out.__bindgen_anon_1.__bindgen_anon_1.size = f.size as i32;
        out.__bindgen_anon_1.__bindgen_anon_1.align = f.align as i32;
        out.__bindgen_anon_1.__bindgen_anon_1.filter_type = c::FILTER_OTHER as i32;
        out.__bindgen_anon_1.__bindgen_anon_1.len = f.len as i32;
        out
    }

    /// @C's fields, then the zeroed one that ends them: @N is one more.
    const fn fields<C: EventClass, const N: usize>() -> [c::trace_event_fields; N] {
        assert!(N == C::FIELDS.len() + 1);
        let mut out: [c::trace_event_fields; N] = ::pin_init::zeroed();
        let mut i = 0;
        while i < C::FIELDS.len() {
            out[i] = field(&C::FIELDS[i]);
            i += 1;
        }
        out
    }

    /// @C's PRINT_FMT, to be written: the kernel edits it in place, as it does a
    /// C event's print_fmt_<call>[]. @N is its length with the NUL.
    const fn print_fmt<C: EventClass, const N: usize>() -> [u8; N] {
        let fmt = C::PRINT_FMT.to_bytes_with_nul();
        assert!(N == fmt.len());
        let mut out = [0; N];
        let mut i = 0;
        while i < N {
            out[i] = fmt[i];
            i += 1;
        }
        out
    }

    /// Class @C's statics, @F its fields with the one that ends them and @P its
    /// print format's bytes: event_class!()'s.
    #[doc(hidden)]
    #[repr(C)]
    pub struct Class<C, const F: usize, const P: usize> {
        class:     UnsafeCell<c::trace_event_class>,
        fields:    UnsafeCell<[c::trace_event_fields; F]>,
        funcs:     UnsafeCell<c::trace_event_functions>,
        print_fmt: UnsafeCell<[u8; P]>,
        _c:        PhantomData<fn() -> C>,
    }

    unsafe impl<C, const F: usize, const P: usize> Sync for Class<C, F, P> {}

    impl<C: EventClass, const F: usize, const P: usize> Class<C, F, P> {
        /// The statics at @this.
        pub const fn new(this: &Self) -> Self {
            let list = unsafe { &raw mut (*this.class.get()).fields };
            Self {
                class: UnsafeCell::new(c::trace_event_class {
                    system:       C::SYSTEM.as_ptr().cast(),
                    probe:        probe::<C> as *mut c_void,
                    #[cfg(CONFIG_PERF_EVENTS)]
                    perf_probe:   perf_probe::<C> as *mut c_void,
                    reg:          Some(c::trace_event_reg),
                    fields_array: this.fields.get().cast(),
                    fields:       c::list_head { next: list, prev: list },
                    raw_init:     Some(c::trace_event_raw_init),
                    ..::pin_init::zeroed()
                }),
                fields:    UnsafeCell::new(fields::<C, F>()),
                funcs:     UnsafeCell::new(c::trace_event_functions {
                    trace: Some(output::<C>),
                    ..::pin_init::zeroed()
                }),
                print_fmt: UnsafeCell::new(print_fmt::<C, P>()),
                _c:        PhantomData,
            }
        }
    }

    #[doc(hidden)]
    pub const fn event<C: EventClass, const F: usize, const P: usize>(
        tp: &Tracepoint<C>, class: &Class<C, F, P>) -> c::trace_event_call {
        let mut call: c::trace_event_call = ::pin_init::zeroed();
        call.class = class.class.get();
        call.__bindgen_anon_1.tp = tp.as_ptr();
        call.event.funcs = class.funcs.get();
        call.print_fmt = class.print_fmt.get().cast();
        call.flags = c::TRACE_EVENT_FL_TRACEPOINT as i32;
        call
    }

    #[cfg(CONFIG_BPF_EVENTS)]
    #[doc(hidden)]
    pub const fn bpf_raw_event_map<C: EventClass>(tp: &Tracepoint<C>) -> c::bpf_raw_event_map {
        c::bpf_raw_event_map {
            tp:       tp.as_ptr(),
            bpf_func: bpf_trace as *mut c_void,
            num_args: 2,
            ..::pin_init::zeroed()
        }
    }

    /// Whether tracepoint @tp is on: trace_<name>_enabled(), a static branch on
    /// its key.
    #[macro_export]
    macro_rules! tracepoint_enabled {
        ($tp:path) => {{
            let _: &'static $crate::os::tracepoint::Tracepoint<_> = &$tp;
            unsafe { $crate::static_branch_unlikely!($tp, $crate::c::tracepoint, key) }
        }};
    }

    /// Class @class's statics, for its tracepoints' events.
    #[macro_export]
    macro_rules! event_class {
        ($class:ident) => { ::paste::paste! {
            #[allow(non_upper_case_globals)]
            static [<__event_class_ $class>]: $crate::os::tracepoint::Class<$class,
                { <$class as $crate::os::tracepoint::EventClass>::FIELDS.len() + 1 },
                { <$class as $crate::os::tracepoint::EventClass>::PRINT_FMT.to_bytes_with_nul().len() }> =
                $crate::os::tracepoint::Class::new(&[<__event_class_ $class>]);
        } };
    }

    /// Tracepoint @name, __tracepoint_<name>, with its event, of class @class,
    /// whose event_class!() is in scope.
    #[macro_export]
    macro_rules! tracepoint {
        ($vis:vis $class:ident, $name:ident) => { ::paste::paste! {
            #[allow(non_upper_case_globals)]
            $vis static [<__tracepoint_ $name>]: $crate::os::tracepoint::Tracepoint<$class> =
                $crate::os::tracepoint::Tracepoint::new(
                    $crate::util::ffi::c_str(concat!(stringify!($name), "\0")));

            #[allow(non_upper_case_globals)]
            static [<event_ $name>]: $crate::os::tracepoint::TracingStatic<$crate::c::trace_event_call> =
                $crate::os::tracepoint::TracingStatic::new($crate::os::tracepoint::event(
                    &[<__tracepoint_ $name>], &[<__event_class_ $class>]));

            #[allow(non_upper_case_globals)]
            #[used(compiler)]
            #[unsafe(link_section = "_ftrace_events")]
            static [<__event_ $name>]: $crate::os::tracepoint::TracingStatic<*mut $crate::c::trace_event_call> =
                $crate::os::tracepoint::TracingStatic::new([<event_ $name>].get());

            #[cfg(CONFIG_HAVE_ARCH_PREL32_RELOCATIONS)]
            ::core::arch::global_asm!(
                ".section \"__tracepoints_ptrs\", \"a\"",
                ".balign 4",
                ".long {tp} - .",
                ".previous",
                tp = sym [<__tracepoint_ $name>],
            );

            #[cfg(not(CONFIG_HAVE_ARCH_PREL32_RELOCATIONS))]
            #[allow(non_upper_case_globals)]
            #[used(compiler)]
            #[unsafe(link_section = "__tracepoints_ptrs")]
            static [<__tracepoint_ptr_ $name>]: $crate::os::tracepoint::TracingStatic<*mut $crate::c::tracepoint> =
                $crate::os::tracepoint::TracingStatic::new([<__tracepoint_ $name>].as_ptr());

            #[cfg(CONFIG_BPF_EVENTS)]
            #[allow(non_upper_case_globals)]
            #[used(compiler)]
            #[unsafe(link_section = "__bpf_raw_tp_map")]
            static [<__bpf_trace_tp_map_ $name>]: $crate::os::tracepoint::TracingStatic<$crate::c::bpf_raw_event_map> =
                $crate::os::tracepoint::TracingStatic::new(
                    $crate::os::tracepoint::bpf_raw_event_map(&[<__tracepoint_ $name>]));
        } };
    }
}
