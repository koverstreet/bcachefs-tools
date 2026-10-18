// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::fs::Fs;
use crate::util::Printbuf;
use crate::types::c_default;
use core::ffi::{c_uint, c_void};
use cstruct_macros::{c_enum, c_extern, CStruct};

#[repr(C)]
#[derive(CStruct)]
pub struct btree_write_bio {
    pub work: c::work_struct,
    pub key: c::bkey_i,
    pub key_pad: [u64; c::BKEY_BTREE_PTR_VAL_U64s_MAX],
    pub data: *mut c_void,
    pub data_bytes: c_uint,
    pub sector_offset: c_uint,
    pub start_time: u64,
    /* C's CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS, which bcachefs.h defines from these */
    #[cfg(all(__KERNEL__, CONFIG_DEBUG_FS))]
    pub list_idx: c_uint,
    pub wbio: c::bch_write_bio,
}
c_default!(btree_write_bio);

c_enum! {
    #[closed]
    pub enum btree_write_flags: u32 {
        __BTREE_WRITE_only_if_need = c::BTREE_WRITE_TYPE_BITS,
        __BTREE_WRITE_already_started,
    }
}

c_extern! {
    pub fn bch2_btree_write_stats_to_text(out: *mut c::printbuf, c: *mut c::bch_fs);
}

impl Fs {
    pub fn btree_write_stats_to_text(&self, out: &mut Printbuf) {
        unsafe { bch2_btree_write_stats_to_text(out.as_raw(), self.as_ptr()) }
    }
}
