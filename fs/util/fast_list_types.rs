// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{c_default, c_opaque};
use crate::util::ffi::Opaque;
use crate::util::genradix::SharedGenRadix;
use core::ffi::c_void;
use core::sync::atomic::AtomicPtr;
use cstruct_macros::CStruct;

// A CPU's cache of free slots: fast_list.rs's Cache - C only holds the pointer
c_opaque!(fast_list_pcpu);

#[repr(C)]
#[derive(CStruct)]
pub struct fast_list {
    /* the items, by slot: allocated by any CPU at once */
    #[c("GENRADIX(void *) items")]
    pub items: SharedGenRadix<*mut c_void>,
    #[c("struct ida slots_allocated")]
    pub slots_allocated: Opaque<c::ida>,
    /* set by init() and exit(), which take the list shared, as C's pointer is */
    #[c("struct fast_list_pcpu __percpu *buffer")]
    pub buffer: AtomicPtr<c::fast_list_pcpu>,
}
c_default!(fast_list);

// SAFETY: zeroed is a valid uninitialized ida, as in C's kzalloc()'d struct;
// fast_list_init() initializes it
unsafe impl ::pin_init::Zeroable for Opaque<c::ida> {}
