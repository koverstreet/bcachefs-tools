// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
#[cfg(CONFIG_DEBUG_FS)]
use crate::util::ffi::c_char;
#[cfg(CONFIG_DEBUG_FS)]
use core::ffi::c_uint;
use cstruct_macros::{CStruct, c_extern};

#[cfg(CONFIG_DEBUG_FS)]
#[repr(C)]
#[derive(CStruct)]
pub struct dump_iter {
    pub c: *mut c::bch_fs,
    pub list: *mut c::async_obj_list,
    pub id: c::btree_id,
    pub level: c_uint,
    pub from: c::bpos,
    pub prev_node: c::bpos,
    pub iter: u64,

    pub buf: c::printbuf,

    #[c("char __user *ubuf")]
    pub ubuf: *mut c_char, /* destination user buffer */
    pub size: usize, /* size of requested read */
    pub ret: isize, /* bytes read so far */
}
#[cfg(CONFIG_DEBUG_FS)]
c_default!(dump_iter);

c_extern! {
    pub fn bch2_btree_node_ondisk_to_text(out: *mut c::printbuf, c: *mut c::bch_fs, b: *const c::btree);
}
