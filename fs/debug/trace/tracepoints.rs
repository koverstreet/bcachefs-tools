// SPDX-License-Identifier: GPL-2.0

//! The tracepoints, and their events: what TRACE_EVENT() makes, for the one
//! class every event is, fs_str - the filesystem's name and a string.
//!
//! The module loader finds each through its sections, as C's: its tracepoint
//! in __tracepoints_ptrs, its event in _ftrace_events, and its raw BPF
//! tracepoint in __bpf_raw_tp_map. A tracepoint has no static call - its
//! static_call_key NULL, which the kernel takes as it does a synthetic
//! event's - so call() walks its probes as __traceiter_<name>() does.

use core::cell::UnsafeCell;
use core::ffi::CStr;
use core::mem::{offset_of, size_of, transmute, MaybeUninit};
use core::ptr;

use crate::c;
use crate::util::ffi::{c_char, c_int, c_str, c_ulong, c_void};

/// A static the kernel's tracing writes, under its own locks.
#[repr(transparent)]
pub(super) struct TracingStatic<T>(UnsafeCell<T>);

unsafe impl<T> Sync for TracingStatic<T> {}

impl<T> TracingStatic<T> {
    const fn new(v: T) -> Self {
        Self(UnsafeCell::new(v))
    }

    const fn get(&self) -> *mut T {
        self.0.get()
    }
}

// ── The call ─────────────────────────────────────────────────────────────

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
        Self(unsafe { c::rust_srcu_read_lock_fast_notrace(&raw mut c::tracepoint_srcu) })
    }
}

#[cfg(linux_ge = "7.0")]
impl Drop for TracepointSrcu {
    fn drop(&mut self) {
        unsafe { c::rust_srcu_read_unlock_fast_notrace(&raw mut c::tracepoint_srcu, self.0) };
    }
}

/// Call @tp's probes with @fs and @s: __do_trace_<name>(). Before 7.0 they run
/// with preemption off; from 7.0 under tracepoint_srcu, preemptible, and the
/// event's probes turn it off themselves.
pub(super) fn call(tp: &TracingStatic<c::tracepoint>, fs: &CStr, s: &CStr) {
    #[cfg(linux_ge = "7.0")]
    let _guard = TracepointSrcu::read_lock();
    #[cfg(not(linux_ge = "7.0"))]
    let _guard = PreemptNotrace::disable();

    let mut it = unsafe { ptr::read_volatile(&raw const (*tp.get()).funcs) };
    if it.is_null() {
        return;
    }
    loop {
        unsafe {
            let probe = transmute::<*mut c_void, Probe>(ptr::read_volatile(&raw const (*it).func));
            probe((*it).data, fs.as_ptr().cast(), s.as_ptr().cast());
            it = it.add(1);
            if (*it).func.is_null() {
                break;
            }
        }
    }
}

// ── The event class: fs_str ──────────────────────────────────────────────

/// __array(char, fs, 40): the filesystem's name.
const FS_LEN: usize = 40;

/// struct trace_event_raw_fs_str: the event as it's in the ring buffer. The
/// kernel lays out its fields from FIELDS, each aligned after the last.
#[repr(C)]
struct Entry {
    ent:     c::trace_entry,
    fs:      [c_char; FS_LEN],
    /// __string(str): str's offset in the entry, its length above 16 bits
    str_loc: u32,
    data:    [u8; 0],
}

const _: () = assert!(offset_of!(Entry, fs) == size_of::<c::trace_entry>());
const _: () = assert!(offset_of!(Entry, str_loc) == offset_of!(Entry, fs) + FS_LEN);

/// TP_fast_assign(): @fs, truncated as strscpy() does, and @s after the entry.
///
/// # Safety
/// @entry has room for an Entry and @s.
unsafe fn assign(entry: *mut Entry, fs: *const c_char, s: &[u8]) {
    let fs = unsafe { CStr::from_ptr(fs.cast()) }.to_bytes();
    let n = fs.len().min(FS_LEN - 1);
    unsafe {
        let dst = (&raw mut (*entry).fs).cast::<u8>();
        dst.copy_from_nonoverlapping(fs.as_ptr(), n);
        dst.add(n).write(0);
        (&raw mut (*entry).str_loc).write(offset_of!(Entry, data) as u32 | ((s.len() as u32) << 16));
        (&raw mut (*entry).data).cast::<u8>().copy_from_nonoverlapping(s.as_ptr(), s.len());
    }
}

/// The ftrace probe: trace_event_raw_event_fs_str().
unsafe extern "C" fn probe(data: *mut c_void, fs: *const c_char, s: *const c_char) {
    #[cfg(linux_ge = "7.0")]
    let _preempt = PreemptNotrace::disable();

    let file = data.cast::<c::trace_event_file>();
    if unsafe { c::trace_trigger_soft_disabled(file) } {
        return;
    }

    let s = unsafe { CStr::from_ptr(s.cast()) }.to_bytes_with_nul();
    let mut fbuffer = MaybeUninit::<c::trace_event_buffer>::uninit();
    let entry = unsafe {
        c::trace_event_buffer_reserve(fbuffer.as_mut_ptr(), file, (size_of::<Entry>() + s.len()) as _)
    }.cast::<Entry>();
    if entry.is_null() {
        return;
    }
    unsafe {
        assign(entry, fs, s);
        c::trace_event_buffer_commit(fbuffer.as_mut_ptr());
    }
}

/// The perf probe: perf_trace_fs_str().
#[cfg(CONFIG_PERF_EVENTS)]
unsafe extern "C" fn perf_probe(data: *mut c_void, fs: *const c_char, s: *const c_char) {
    #[cfg(linux_ge = "7.0")]
    let _preempt = PreemptNotrace::disable();

    let call = data.cast::<c::trace_event_call>();
    let head = unsafe { crate::util::percpu::bch2_this_cpu_ptr((*call).perf_events.cast()) }
        .cast::<c::hlist_head>();
    if !unsafe { c::bpf_prog_array_valid(call) } &&
        unsafe { ptr::read_volatile(&raw const (*head).first) }.is_null() {
        return;
    }

    let s = unsafe { CStr::from_ptr(s.cast()) }.to_bytes_with_nul();
    let size = (size_of::<Entry>() + s.len() + size_of::<u32>()).next_multiple_of(size_of::<u64>())
        - size_of::<u32>();
    let mut regs = ptr::null_mut();
    let mut rctx = 0;
    let entry = unsafe { c::perf_trace_buf_alloc(size as c_int, &mut regs, &mut rctx) }.cast::<Entry>();
    if entry.is_null() {
        return;
    }
    unsafe {
        c::rust_perf_fetch_caller_regs(regs);
        assign(entry, fs, s);
        c::perf_trace_run_bpf_submit(entry.cast(), size as c_int, rctx, call, 1, regs, head,
                                     ptr::null_mut());
    }
}

/// The raw BPF tracepoint's: __bpf_trace_fs_str().
#[cfg(CONFIG_BPF_EVENTS)]
unsafe extern "C" fn bpf_trace(data: *mut c_void, fs: *const c_char, s: *const c_char) {
    unsafe { c::bpf_trace_run2(data.cast(), fs as u64, s as u64) };
}

/// The event's output, trace_raw_output_fs_str(): TP_printk().
unsafe extern "C" fn output(iter: *mut c::trace_iterator, _flags: c_int,
                            event: *mut c::trace_event) -> c::print_line_t {
    let ret = unsafe { c::trace_raw_output_prep(iter, event) };
    if ret != c::print_line_t_TRACE_TYPE_HANDLED {
        return ret;
    }
    unsafe {
        let entry = (*iter).ent.cast::<Entry>();
        let s = entry.cast::<u8>().add(((*entry).str_loc & 0xffff) as usize);
        c::trace_event_printf(iter, c"%s: %s\n".as_ptr().cast(), (&raw const (*entry).fs).cast::<c_char>(), s);
        c::trace_handle_return(&raw mut (*iter).seq)
    }
}

/// TP_printk()'s, as the event's format file shows it.
static PRINT_FMT: TracingStatic<[u8; 34]> = TracingStatic::new(*b"\"%s: %s\", REC->fs, __get_str(str)\0");

/// TP_STRUCT__entry()'s. Not signed: the kernel's char is unsigned.
const fn field(ty: &CStr, name: &CStr, size: usize, align: usize, len: usize) -> c::trace_event_fields {
    let mut f: c::trace_event_fields = ::pin_init::zeroed();
    f.type_ = ty.as_ptr().cast();
    f.__bindgen_anon_1.__bindgen_anon_1.name = name.as_ptr().cast();
    f.__bindgen_anon_1.__bindgen_anon_1.size = size as c_int;
    f.__bindgen_anon_1.__bindgen_anon_1.align = align as c_int;
    f.__bindgen_anon_1.__bindgen_anon_1.filter_type = c::FILTER_OTHER as c_int;
    f.__bindgen_anon_1.__bindgen_anon_1.len = len as c_int;
    f
}

static FIELDS: TracingStatic<[c::trace_event_fields; 3]> = TracingStatic::new([
    field(c"char[40]", c"fs", FS_LEN, 1, FS_LEN),
    field(c"__data_loc char[]", c"str", 4, 4, 0),
    ::pin_init::zeroed(),
]);

static FUNCS: TracingStatic<c::trace_event_functions> = TracingStatic::new(c::trace_event_functions {
    trace: Some(output),
    ..::pin_init::zeroed()
});

static CLASS: TracingStatic<c::trace_event_class> = TracingStatic::new(c::trace_event_class {
    system:       c"bcachefs".as_ptr().cast(),
    probe:        probe as *mut c_void,
    #[cfg(CONFIG_PERF_EVENTS)]
    perf_probe:   perf_probe as *mut c_void,
    reg:          Some(c::trace_event_reg),
    fields_array: FIELDS.get().cast(),
    fields:       {
        let fields = unsafe { &raw mut (*CLASS.get()).fields };
        c::list_head { next: fields, prev: fields }
    },
    raw_init:     Some(c::trace_event_raw_init),
    ..::pin_init::zeroed()
});

// ── The tracepoints and events ───────────────────────────────────────────

const fn tracepoint(name: &'static CStr) -> c::tracepoint {
    c::tracepoint { name: name.as_ptr().cast(), ..::pin_init::zeroed() }
}

const fn event(tp: *mut c::tracepoint) -> c::trace_event_call {
    let mut call: c::trace_event_call = ::pin_init::zeroed();
    call.class = CLASS.get();
    call.__bindgen_anon_1.tp = tp;
    call.event.funcs = FUNCS.get();
    call.print_fmt = PRINT_FMT.get().cast();
    call.flags = c::TRACE_EVENT_FL_TRACEPOINT as c_int;
    call
}

#[cfg(CONFIG_BPF_EVENTS)]
const fn bpf_raw_event_map(tp: *mut c::tracepoint) -> c::bpf_raw_event_map {
    c::bpf_raw_event_map { tp, bpf_func: bpf_trace as *mut c_void, num_args: 2, ..::pin_init::zeroed() }
}

macro_rules! tracepoints {
    ($(($name:ident $(, $($rest:tt)*)?)),* $(,)?) => { ::paste::paste! { $(
        #[allow(non_upper_case_globals)]
        pub(super) static [<__tracepoint_ $name>]: TracingStatic<c::tracepoint> =
            TracingStatic::new(tracepoint(c_str(concat!(stringify!($name), "\0"))));

        #[allow(non_upper_case_globals)]
        static [<event_ $name>]: TracingStatic<c::trace_event_call> =
            TracingStatic::new(event([<__tracepoint_ $name>].get()));

        #[allow(non_upper_case_globals)]
        #[used(compiler)]
        #[unsafe(link_section = "_ftrace_events")]
        static [<__event_ $name>]: TracingStatic<*mut c::trace_event_call> =
            TracingStatic::new([<event_ $name>].get());

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
        static [<__tracepoint_ptr_ $name>]: TracingStatic<*mut c::tracepoint> =
            TracingStatic::new([<__tracepoint_ $name>].get());

        #[cfg(CONFIG_BPF_EVENTS)]
        #[allow(non_upper_case_globals)]
        #[used(compiler)]
        #[unsafe(link_section = "__bpf_raw_tp_map")]
        static [<__bpf_trace_tp_map_ $name>]: TracingStatic<c::bpf_raw_event_map> =
            TracingStatic::new(bpf_raw_event_map([<__tracepoint_ $name>].get()));
    )* } };
}
c::bch_persistent_counters_arms!(tracepoints);
c::BCH_NOCOUNTER_TRACEPOINTS!(tracepoints);
#[cfg(CONFIG_BCACHEFS_PATH_TRACEPOINTS)]
c::BCH_PATH_TRACEPOINTS!(tracepoints);
