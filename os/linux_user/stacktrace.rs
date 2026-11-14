// SPDX-License-Identifier: GPL-2.0

//! A task's stack, for userspace: unwound by the backtrace crate - another
//! thread's, in that thread, from a signal handler - and an address's
//! symbol, from the binary's debug info.

use crate::c;
use core::cell::Cell;
use core::ffi::{c_int, c_ulong, c_void};
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering::{Acquire, Relaxed, Release}};
use std::sync::{Mutex, Once};

/*
 * The unwind has to run in the target thread's own context, which means
 * a signal handler. We use SIGRTMIN: real-time, designed for app use,
 * less likely to collide with anything else than SIGUSR{1,2}.
 *
 * Concurrency: a single in-flight slot, serialized by BT_LOCK. Debug
 * paths only — there's never enough contention to make this hot.
 */

struct Request {
    store:  *mut c_ulong,
    size:   usize,
    skipnr: u32,
    target: c_int,
    nr:     AtomicUsize,
    done:   AtomicBool,
}

static BT_LOCK: Mutex<()> = Mutex::new(());
static BT_IN_FLIGHT: AtomicPtr<Request> = AtomicPtr::new(ptr::null_mut());
static BT_HANDLER: Once = Once::new();

fn gettid() -> c_int {
    rustix::thread::gettid().as_raw_nonzero().get()
}

/// This thread's stack into the @size at @store: from the frame after
/// @own's - the function @own is the address of - and @skipnr more.
fn unwind(store: *mut c_ulong, size: usize, skipnr: u32, own: usize) -> usize {
    let (mut nr, mut skip, mut found) = (0, skipnr, false);

    unsafe {
        backtrace::trace_unsynchronized(|frame| {
            if !found {
                found = frame.symbol_address().addr() == own;
            } else if skip != 0 {
                skip -= 1;
            } else {
                store.add(nr).write(frame.ip().addr() as c_ulong);
                nr += 1;
            }
            nr < size
        });
    }
    nr
}

extern "C" fn bt_signal_handler(_sig: c_int) {
    let r = BT_IN_FLIGHT.load(Acquire);
    let Some(r) = (unsafe { r.as_ref() }) else { return };
    if gettid() != r.target {
        return;
    }

    let nr = unwind(r.store, r.size, r.skipnr, bt_signal_handler as *const () as usize);
    r.nr.store(nr, Relaxed);
    r.done.store(true, Release);
}

fn bt_install_handler() {
    unsafe {
        let mut sa: libc::sigaction = core::mem::zeroed();
        sa.sa_sigaction = bt_signal_handler as *const () as usize;
        sa.sa_flags     = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGRTMIN(), &sa, ptr::null_mut());
    }
}

/// stack_trace_save_tsk(): @task's stack, @skipnr frames in, into the @size
/// at @store - how many there were.
///
/// # Safety
/// @task is live, and @store has room for @size.
#[inline(never)]
pub unsafe fn save_tsk(task: *mut c::task_struct, store: *mut c_ulong, size: usize, skipnr: u32) -> usize {
    let pid = if task.is_null() { 0 } else { unsafe { (*task).pid } };
    if pid == 0 || size == 0 {
        return 0;
    }

    /* Our own: no signal needed, just walk our own stack. */
    if pid == gettid() {
        return unwind(store, size, skipnr, save_tsk as *const () as usize);
    }

    BT_HANDLER.call_once(bt_install_handler);

    let _lock = BT_LOCK.lock().expect("backtrace lock poisoned");

    let r = Request { store, size, skipnr, target: pid, nr: AtomicUsize::new(0), done: AtomicBool::new(false) };
    BT_IN_FLIGHT.store(ptr::from_ref(&r).cast_mut(), Release);

    if unsafe { libc::tgkill(libc::getpid(), pid, libc::SIGRTMIN()) } == 0 {
        while !r.done.load(Acquire) {
            std::thread::yield_now();
        }
    }

    BT_IN_FLIGHT.store(ptr::null_mut(), Release);
    r.nr.load(Relaxed)
}

/*
 * Symbols are resolved from the debug info, its caches filled as addresses
 * are looked up, unlocked: one thread at a time. A backtrace printed by the
 * thread resolving - from a signal handler, or a panic in the resolver -
 * would wait on itself: it goes with the dynamic symbol table's names - the
 * binary is linked -rdynamic - and no lines.
 */
static RESOLVE_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    static RESOLVING: Cell<bool> = const { Cell::new(false) };
}

fn dladdr_symbol(addr: c_ulong) -> String {
    unsafe {
        let mut info: libc::Dl_info = core::mem::zeroed();
        if libc::dladdr(addr as *const c_void, &mut info) != 0 && !info.dli_sname.is_null() {
            let name = core::ffi::CStr::from_ptr(info.dli_sname).to_string_lossy();
            return format!("{name}+{:#x}", addr - info.dli_saddr.addr() as c_ulong);
        }
    }
    format!("{addr:#x}")
}

/// @addr's symbol: "name+0xoff", mangled as the binary has it, and
/// " file:line" where the debug info has it.
pub fn symbolize(addr: c_ulong) -> String {
    if RESOLVING.get() {
        return dladdr_symbol(addr);
    }
    let _lock = RESOLVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    RESOLVING.set(true);

    let (mut name, mut line) = (None, None);
    /*
     * A frame's address is where the call returns to, which can be the next
     * line, or no line at all: the call is the byte before, as %pB has it.
     * Inlined frames first, innermost first: the line is the innermost's,
     * the name the function's whose code it is - the last's.
     */
    unsafe {
        backtrace::resolve_unsynchronized((addr - 1) as *mut c_void, |sym| {
            if line.is_none() {
                if let (Some(file), Some(n)) = (sym.filename(), sym.lineno()) {
                    line = Some(format!(" {}:{n}", file.display()));
                }
            }
            if let (Some(n), Some(start)) = (sym.name(), sym.addr()) {
                name = Some(format!("{}+{:#x}", String::from_utf8_lossy(n.as_bytes()),
                                    addr.wrapping_sub(start.addr() as c_ulong)));
            }
        });
    }

    RESOLVING.set(false);
    name.unwrap_or_else(|| dladdr_symbol(addr)) + line.as_deref().unwrap_or("")
}
