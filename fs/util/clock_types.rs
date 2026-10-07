// SPDX-License-Identifier: GPL-2.0

//! The data types of util/clock_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{MinHeap, c_default};
use cstruct_macros::{c_const, c_typedef, c_verbatim, CStruct};


c_const! {
    #[c_int]
    pub const NR_IO_TIMERS: u32 = c::BCH_SB_MEMBERS_MAX * 3;
}

c_verbatim!(r#"
/*
 * Clocks/timers in units of sectors of IO:
 *
 * Note - they use percpu batching, so they're only approximate.
 */

struct io_timer;
"#);

c_typedef! {
    pub type io_timer_fn = Option<unsafe extern "C" fn(*mut c::io_timer)>;
}

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct io_timer {
    pub fn_: c::io_timer_fn,
    pub fn2: *mut core::ffi::c_void,
    pub expire: u64,
}
c_default!(io_timer);

c_const! {
    /* Amount to buffer up on a percpu counter */
    #[c_int]
    pub const IO_CLOCK_PCPU_SECTORS: u32 = 128;
}

c_typedef! {
    #[c("DEFINE_MIN_HEAP(struct io_timer *, io_timer_heap) io_timer_heap")]
    pub type io_timer_heap = MinHeap<*mut c::io_timer>;
}

#[repr(C)]
#[derive(CStruct)]
pub struct io_clock {
    pub now: c::atomic64_t,
    #[c("u16 __percpu *pcpu_buf")]
    pub pcpu_buf: *mut u16,
    pub max_slop: core::ffi::c_uint,

    pub timer_lock: c::spinlock_t,
    pub timers: c::io_timer_heap,
}
c_default!(io_clock);
