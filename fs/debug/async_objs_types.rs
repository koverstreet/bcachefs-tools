// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use cstruct_macros::{c_enum, c_xmacro};

// The lists, where Fs has them: only with the lists configured
#[cfg(CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS)]
use {
    crate::cstructs::c,
    crate::fs::Fs,
    core::ffi::{c_uint, c_void},
    cstruct_macros::{placed, CStruct, Init},
    pin_init::pin_data,
};

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

#[cfg(CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS)]
#[placed(Fs.async_objs[], index = idx: c_uint)]
#[pin_data]
#[repr(C)]
#[derive(CStruct, Init)]
pub struct async_obj_list {
    pub list: c::fast_list,
    pub obj_to_text: Option<unsafe extern "C" fn(*mut c::printbuf, *mut c::bch_fs, *mut c_void)>,
}
