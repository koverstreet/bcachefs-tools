// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::CStruct;

#[repr(C)]
#[derive(CStruct)]
pub struct stdio_buf {
    pub lock: c::spinlock_t,
    pub wait: c::wait_queue_head_t,
    pub buf: c::darray_char,
    pub waiting_for_line: bool,
}
c_default!(stdio_buf);

#[repr(C)]
#[derive(Default, CStruct)]
pub struct stdio_redirect {
    pub input: c::stdio_buf,
    pub output: c::stdio_buf,
    pub done: bool,
}
