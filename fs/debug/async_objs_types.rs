// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use core::ffi::{c_uint, c_void};
use cstruct_macros::{c_enum, c_xmacro, CStruct};

c_xmacro! {
    BCH_ASYNC_OBJ_LISTS(x) {
        (promote),
        (rbio),
        (write_op),
        (btree_read_bio),
        (btree_write_bio),
    }
}

c_enum! {
    #[closed]
    pub enum bch_async_obj_lists: u32 {
        BCH_ASYNC_OBJ_LISTS(n) => BCH_ASYNC_OBJ_LIST_ ## n,
        BCH_ASYNC_OBJ_NR,
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct async_obj_list {
    pub list: c::fast_list,
    pub obj_to_text: Option<unsafe extern "C" fn(*mut c::printbuf, *mut c::bch_fs, *mut c_void)>,
    pub idx: c_uint,
}
