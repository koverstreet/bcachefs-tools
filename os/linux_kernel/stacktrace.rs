// SPDX-License-Identifier: GPL-2.0

//! A task's stack: its frames' return addresses.

use crate::c;
use core::ffi::{c_uint, c_ulong};

/// stack_trace_save_tsk(): @task's stack, @skipnr frames in, into the @size
/// at @store - how many there were.
///
/// # Safety
/// @task is live, and @store has room for @size.
pub unsafe fn save_tsk(task: *mut c::task_struct, store: *mut c_ulong, size: usize, skipnr: u32) -> usize {
    unsafe { c::stack_trace_save_tsk(task, store.cast(), size as c_uint, skipnr) as usize }
}
