// SPDX-License-Identifier: GPL-2.0

//! The data types of debug/async_objs_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
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

macro_rules! __bch_async_obj_lists_0 {
    ([$($acc:tt)*] $(($n:tt)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[closed]
            pub enum bch_async_obj_lists: u32 {
                $($acc)*
                $([<BCH_ASYNC_OBJ_LIST_ $n>],)*
                BCH_ASYNC_OBJ_NR,
            }
        }
    } };
}
BCH_ASYNC_OBJ_LISTS!(__bch_async_obj_lists_0 []);

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct async_obj_list {
    pub list: c::fast_list,
    pub obj_to_text: Option<unsafe extern "C" fn(*mut c::printbuf, *mut c::bch_fs, *mut core::ffi::c_void)>,
    pub idx: core::ffi::c_uint,
}
