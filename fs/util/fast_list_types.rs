// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{GenRadix, c_opaque, c_default};
use core::ffi::c_void;
use cstruct_macros::CStruct;

c_opaque!(fast_list_pcpu);

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct fast_list {
    pub items: GenRadix<*mut c_void>,
    pub slots_allocated: c::ida,
    #[c("struct fast_list_pcpu __percpu *buffer")]
    pub buffer: *mut c::fast_list_pcpu,
}
c_default!(fast_list);
