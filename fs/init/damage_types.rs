// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::DArray;
use core::ffi::c_int;
use cstruct_macros::{CStruct, c_extern, c_typedef};

/*
 * The key range of an extents btree node we lost.
 *
 * Stashed at the site that loses the node, because that is the last moment
 * the range is known: afterwards an inode that had every extent in the node
 * is indistinguishable from a sparse one. check_extents turns the stashed
 * ranges into per-inode damage records, and bch2_btree_lost_data() - called
 * at the same sites - is what schedules it.
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
#[c_typedef]
pub struct lost_extents_range {
    pub start: c::bpos,
    pub end: c::bpos,
}

c_typedef! {
    pub type darray_lost_extents_range = DArray<c::lost_extents_range>;
}

c_extern! {
    pub fn bch2_damage_delete(trans: *mut c::btree_trans, inum: u64, snapshot: u32) -> c_int;
    pub fn bch2_damage_record_lost_extents(c: *mut c::bch_fs);
    pub fn bch2_damage_keys_merge(trans: *mut c::btree_trans, pos: c::bpos,
                                  a: c::bkey_s_c, b: c::bkey_s_c) -> *mut c::bkey_i;
}
