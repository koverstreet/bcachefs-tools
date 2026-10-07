// SPDX-License-Identifier: GPL-2.0

//! The data types of util/thread_with_file_defs.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{c_verbatim, CStruct};

c_verbatim!(r#"
/*
 * Thread with file: Run a kthread and connect it to a file descriptor, so that
 * it can be interacted with via fd read/write methods and closing the file
 * descriptor stops the kthread.
 *
 * We have two different APIs:
 *
 * thread_with_file, the low level version.
 * You get to define the full file_operations, including your release function,
 * which means that you must call bch2_thread_with_file_exit() from your
 * .release method
 *
 * thread_with_stdio, the higher level version
 * This implements full piping of input and output, including .poll.
 *
 * Notes on behaviour:
 *  - kthread shutdown behaves like writing or reading from a pipe that has been
 *    closed
 *  - Input and output buffers are 4096 bytes, although buffers may in some
 *    situations slightly exceed that limit so as to avoid chopping off a
 *    message in the middle in nonblocking mode.
 *  - Input/output buffers are lazily allocated, with GFP_NOWAIT allocations -
 *    should be fine but might change in future revisions.
 *  - Output buffer may grow past 4096 bytes to deal with messages that are
 *    bigger than 4096 bytes
 *  - Writing may be done blocking or nonblocking; in nonblocking mode, we only
 *    drop entire messages.
 *
 * To write, use stdio_redirect_printf()
 * To read, use stdio_redirect_read() or stdio_redirect_readline()
 */
struct task_struct;
"#);

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct thread_with_file {
    pub task: *mut c::task_struct,
    pub ret: core::ffi::c_int,
    pub done: bool,
}
c_default!(thread_with_file);

c_verbatim!(r#"
struct thread_with_stdio;
"#);

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct thread_with_stdio_ops {
    pub exit: Option<unsafe extern "C" fn(*mut c::thread_with_stdio)>,
    pub fn_: Option<unsafe extern "C" fn(*mut c::thread_with_stdio) -> core::ffi::c_int>,
    pub unlocked_ioctl: Option<unsafe extern "C" fn(*mut c::thread_with_stdio, core::ffi::c_uint, core::ffi::c_ulong) -> core::ffi::c_long>,
}

#[repr(C)]
#[derive(CStruct)]
pub struct thread_with_stdio {
    pub thr: c::thread_with_file,
    pub stdio: c::stdio_redirect,
    pub ops: *const c::thread_with_stdio_ops,
}
c_default!(thread_with_stdio);
