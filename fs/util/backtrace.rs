// SPDX-License-Identifier: GPL-2.0

//! Backtraces as text: a frame a line, each address's symbol with its name
//! demangled.
//!
//! What resolves an address differs by where we run, and nothing else does.
//! In the kernel it's kallsyms - "name+0xoff/0xsize [module]", as %pB prints it;
//! in userspace, the shim's bch2_prt_symbol(), libunwind and libdw - "name+0xoff
//! file:line". Either way the name comes first and has no '+' in it, and it's
//! mangled where it's Rust's: kallsyms keeps symbols as the compiler emitted
//! them (v0, -Csymbol-mangling-version=v0), and the kernel has no demangler -
//! scripts/decode_stacktrace.sh runs one offline. So the name is demangled
//! here, for both, and the rest of the line printed as it came.
//!
//! Callers print a lock owner's stack from lock debugging - anywhere, as deep
//! as the stack goes - so a symbol costs the stack nothing: kallsyms writes it
//! straight into the printbuf - not through %pB, whose vsnprintf() takes a
//! symbol's length of stack for scratch - and demangling is on the heap
//! (util/demangle.rs), in a NORECLAIM scope, the name printed raw if that
//! fails.

use crate::c;
use crate::prt;
use crate::util::alloc::{AllocError, KVVec};
use crate::util::demangle::prt_demangled;
use crate::os::sched;
use crate::util::Printbuf;
use core::ffi::c_ulong;

/// @sym, a symbol "name..." as the symbolizer printed it, to @out: the name
/// demangled - a Rust name, mangled; anything else as it is.
fn prt_symbol_demangled(out: &mut Printbuf, sym: &str) {
    let (name, rest) = sym.split_at(sym.find('+').unwrap_or(sym.len()));

    prt_demangled(out, name);
    prt!(out, "{rest}");
}

/// kallsyms' text for @addr - 1, @len bytes of @s, made @addr's in place: the
/// offset one more - or, for an address it didn't find, the address. @s has
/// room for the digit a carry adds. The new length.
#[cfg(kernel)]
fn symbol_offset_inc(s: &mut [u8], len: usize) -> usize {
    // "name+0xoff/0xsize [mod]", or "0xaddr"
    let text = &s[..len];
    let digits = match text.iter().position(|&c| c == b'+') {
        Some(p) => p + 3..text[p..].iter().position(|&c| c == b'/').map_or(len, |i| p + i),
        None => 2..len,
    };
    if digits.is_empty() || !s[digits.clone()].iter().all(u8::is_ascii_hexdigit) {
        return len;
    }

    for i in digits.clone().rev() {
        match s[i] {
            b'f' => s[i] = b'0',
            b'9' => { s[i] = b'a'; return len; }
            c => { s[i] = c + 1; return len; }
        }
    }
    s.copy_within(digits.start..len, digits.start + 1);
    s[digits.start] = b'1';
    len + 1
}

/// @addr's symbol, demangled, to @out.
#[cfg(kernel)]
pub(crate) fn prt_symbol(out: &mut Printbuf, addr: c_ulong) {
    let start = out.as_bytes().len();

    // %pB's lookup, sprint_backtrace(), isn't exported: sprint_symbol() of
    // @addr - 1, so a return address past a noreturn call is its caller's.
    // It writes KSYM_SYMBOL_LEN at most, its nul included, and
    // symbol_offset_inc() a byte more.
    let written = unsafe {
        out.write_in_place(c::KSYM_SYMBOL_LEN as usize, |buf| {
            let len = c::sprint_symbol(buf.cast(), addr.wrapping_sub(1) as _) as usize;
            symbol_offset_inc(core::slice::from_raw_parts_mut(buf, len + 1), len)
        })
    };
    if !written {
        prt!(out, "{addr:#x}");
        return;
    }

    // demangling reads the name as it writes: a Rust name is moved out of
    // @out first, and left as it is if it can't be
    let mut bounce = KVVec::new();
    {
        let sym = &out.as_bytes()[start..];
        if !sym.starts_with(b"_R") {
            return;
        }
        let _noreclaim = crate::locking::MemallocFlags::noreclaim();
        if bounce.extend_from_slice(sym).is_err() {
            return;
        }
    }
    let Ok(sym) = core::str::from_utf8(&bounce) else { return };

    out.truncate(start);
    prt_symbol_demangled(out, sym);
}

/// @addr's symbol, demangled, to @out.
#[cfg(not(kernel))]
pub(crate) fn prt_symbol(out: &mut Printbuf, addr: c_ulong) {
    prt_symbol_demangled(out, &crate::os::stacktrace::symbolize(addr));
}

/// @stack, a frame a line, as the kernel prints them: "[<0>] " and the
/// frame's symbol.
pub fn prt_backtrace(out: &mut Printbuf, stack: &[c_ulong]) {
    for &addr in stack {
        prt!(out, "[<0>] ");
        prt_symbol(out, addr);
        out.newline();
    }
}

/// @task's stack, @skipnr frames in - and the frame saving it, for current -
/// into @stack, grown till it fits: an error if it can't be, what fit kept.
pub fn save_backtrace(stack: &mut KVVec<c_ulong>, task: *mut c::task_struct, skipnr: u32) -> Result<(), AllocError> {
    #[cfg(CONFIG_STACKTRACE)]
    {
        stack.clear();
        stack.reserve(32)?;

        let skipnr = skipnr + (task == sched::current()) as u32;
        loop {
            let size = stack.capacity();
            stack.clear();
            let nr = unsafe { crate::os::stacktrace::save_tsk(task, stack.as_mut_ptr(), size, skipnr) };
            unsafe { stack.inc_len(nr) };
            if nr < size {
                return Ok(());
            }
            stack.reserve(size)?;
        }
    }
    #[cfg(not(CONFIG_STACKTRACE))]
    {
        let _ = (stack, task, skipnr);
        Ok(())
    }
}

/// @task's stack, @skipnr frames in, to @out: what fit, if growing to fit
/// failed.
pub fn prt_task_backtrace(out: &mut Printbuf, task: *mut c::task_struct, skipnr: u32) -> Result<(), AllocError> {
    let skipnr = skipnr + (task == sched::current()) as u32;

    let mut stack = KVVec::new();
    let ret = save_backtrace(&mut stack, task, skipnr);
    prt_backtrace(out, &stack);
    ret
}

pub fn prt_current_backtrace(out: &mut Printbuf, skipnr: u32) -> Result<(), AllocError> {
    prt_task_backtrace(out, sched::current(), skipnr)
}

/// The shim's dump_stack(), to stderr.
#[cfg(not(kernel))]
#[unsafe(no_mangle)]
pub extern "C" fn dump_stack() {
    use std::io::Write;

    let mut out = Printbuf::new();
    let _ = prt_task_backtrace(&mut out, sched::current(), 0);
    let _ = std::io::stderr().write_all(out.as_str().as_bytes());
}
