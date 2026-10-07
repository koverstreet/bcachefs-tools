// SPDX-License-Identifier: GPL-2.0

//! The data types of init/damage_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::DArray;
use cstruct_macros::{CStruct, c_extern, c_verbatim};


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

c_verbatim!(r#"
DEFINE_DARRAY(lost_extents_range);
"#);

pub type darray_lost_extents_range = DArray<c::lost_extents_range>;

// What Rust calls of init/damage.h: C gets these as prototypes, in init/damage_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_damage_delete(arg1: *mut c::btree_trans, arg2: u64, arg3: u32) -> core::ffi::c_int;
    pub fn bch2_damage_record_lost_extents(arg1: *mut c::bch_fs);
}
