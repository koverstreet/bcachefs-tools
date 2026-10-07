// SPDX-License-Identifier: GPL-2.0

//! The data types of util/fast_list_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{GenRadix, c_opaque, c_default};
use cstruct_macros::{c_verbatim, CStruct};

c_verbatim!(r#"
struct fast_list_pcpu;
"#);

c_opaque!(fast_list_pcpu);

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct fast_list {
    #[c("GENRADIX(void *) items")]
    pub items: GenRadix<*mut core::ffi::c_void>,
    pub slots_allocated: c::ida,
    #[c("struct fast_list_pcpu __percpu *buffer")]
    pub buffer: *mut c::fast_list_pcpu,
}
c_default!(fast_list);
