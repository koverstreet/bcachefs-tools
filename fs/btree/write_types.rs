// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::btree::bkey::BKEY_U64S;
use crate::cstructs::c;
use crate::data::keylist::PaddedKey;
use crate::fs::Fs;
use crate::util::Printbuf;
use crate::types::c_default;
use core::ffi::{c_uint, c_void};
use cstruct_macros::{c_enum, c_extern, CStruct};

#[repr(C)]
#[derive(CStruct)]
pub struct btree_write_bio {
    pub work: c::work_struct,
    #[c_anon("struct { struct bkey_i key; u64 key_pad[BKEY_BTREE_PTR_VAL_U64s_MAX]; }")]
    pub key: PaddedKey<{ BKEY_U64S + c::BKEY_BTREE_PTR_VAL_U64s_MAX }>,
    pub data: *mut c_void,
    pub data_bytes: c_uint,
    pub sector_offset: c_uint,
    pub start_time: u64,
    /* its slot on its async object list */
    pub list_idx: c_uint,
    pub failed: c::bch_io_failures,
    pub wbio: c::bch_write_bio,
}
c_default!(btree_write_bio);
crate::has_bio!(btree_write_bio, wbio.bio);

c_enum! {
    #[closed]
    pub enum btree_write_flags: u32 {
        __BTREE_WRITE_only_if_need = c::BTREE_WRITE_TYPE_BITS,
        __BTREE_WRITE_already_started,
    }
}

c_extern! {
    pub fn bch2_btree_write_stats_to_text(out: *mut Printbuf, c: *mut Fs);
    pub fn bch2_btree_flush_all_writes(c: *mut Fs) -> bool;
    pub fn bch2_btree_cancel_all_writes(c: *mut Fs);
}

impl Fs {
    pub fn btree_write_stats_to_text(&self, out: &mut Printbuf) {
        unsafe { bch2_btree_write_stats_to_text(out.as_raw(), self.as_ptr()) }
    }
}
