// SPDX-License-Identifier: GPL-2.0

//! Backtraces as text: a frame a line, each address's symbol with its name
//! demangled.
//!
//! What resolves an address differs by where we run, and nothing else does.
//! In the kernel it's kallsyms, through %pB - "name+0xoff/0xsize [module]";
//! in userspace, the shim's bch2_prt_symbol(), libunwind and libdw - "name+0xoff
//! file:line". Either way the name comes first and has no '+' in it, and it's
//! mangled where it's Rust's: kallsyms keeps symbols as the compiler emitted
//! them (v0, -Csymbol-mangling-version=v0), and the kernel has no demangler -
//! scripts/decode_stacktrace.sh runs one offline. So the name is demangled
//! here, for both, and the rest of the line printed as it came.
//!
//! Callers print a lock owner's stack from lock debugging - anywhere, as deep
//! as the stack goes - so a symbol costs the stack little: the kernel's
//! scratch for it is on the stack, a symbol's length, and demangling is on
//! the heap (util/demangle.rs), in a NORECLAIM scope, the name printed raw if
//! that fails.

use crate::c;
use crate::errcode::ret_to_c;
use crate::prt;
use crate::locking::MemallocFlags;
use crate::util::alloc::{AllocError, KVVec};
use crate::util::demangle::prt_demangled;
use crate::util::sched;
use crate::util::Printbuf;
use core::ffi::{c_int, c_uint, c_ulong};
use cstruct_macros::{c_export, c_extern};

// bch2_prt_symbol(): @addr's symbol, "name+0xoff", and " file:line" if libdw
// knows it - linux/stacktrace.c, the userspace shim's %pB.
c_extern! {
    #[cfg(not(kernel))]
    pub fn bch2_prt_symbol(out: *mut Printbuf, addr: c_ulong);
}

/// @sym, a symbol "name..." as the symbolizer printed it, to @out: the name
/// demangled - a Rust name, mangled; anything else as it is.
fn prt_symbol_demangled(out: &mut Printbuf, sym: &str) {
    let (name, rest) = sym.split_at(sym.find('+').unwrap_or(sym.len()));

    prt_demangled(out, name);
    prt!(out, "{rest}");
}

/// @addr's symbol, demangled, to @out.
#[cfg(kernel)]
pub(crate) fn prt_symbol(out: &mut Printbuf, addr: c_ulong) {
    let mut buf = [0u8; c::KSYM_SYMBOL_LEN as usize];
    // SAFETY: @buf outlives @sym, which writes nothing past it
    let mut sym = unsafe { Printbuf::new_extern(buf.as_mut_ptr().cast(), buf.len()) };

    // SAFETY: our printbuf; %pB takes the address as a pointer
    unsafe { c::bch2_prt_printf(sym.as_raw(), c"%pB".as_ptr().cast(), addr as *const core::ffi::c_void) };
    prt_symbol_demangled(out, sym.as_str());
}

/// @addr's symbol, demangled, to @out.
#[cfg(not(kernel))]
pub(crate) fn prt_symbol(out: &mut Printbuf, addr: c_ulong) {
    let mut sym = Printbuf::new();

    // SAFETY: our printbuf
    unsafe { bch2_prt_symbol(sym.as_raw(), addr) };
    prt_symbol_demangled(out, sym.as_str());
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
            let nr = unsafe { c::stack_trace_save_tsk(task, stack.as_mut_ptr().cast(), size as c_uint, skipnr) } as usize;
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

#[c_export]
pub fn bch2_prt_task_backtrace(out: &mut Printbuf, task: *mut c::task_struct, skipnr: c_uint, gfp: c::gfp_t)
    -> c_int
{
    let _scope = MemallocFlags::for_gfp(gfp);
    ret_to_c(prt_task_backtrace(out, task, skipnr))
}

#[c_export]
pub fn bch2_prt_backtrace(out: &mut Printbuf, stack: &c::bch_stacktrace) {
    prt_backtrace(out, stack);
}
