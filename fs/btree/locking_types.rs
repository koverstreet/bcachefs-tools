// SPDX-License-Identifier: GPL-2.0

//! The data types of btree/locking_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{DArray, c_default};
use cstruct_macros::{bitfield, c_extern, CStruct};

// What Rust calls of btree/locking.h: C gets these as prototypes, in
// btree/locking_gen.h - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn rust_bch2_inode_shard_idx(c: *mut c::bch_fs) -> u64;
}

#[bitfield(u8)]
pub struct trans_waiting_for_lock_lock_want_bits {
    #[bits(8)]
    pub lock_want: u32,
}

/* State used for the cycle detector */
/*
 * @trans wants to lock @b with type @type
 */
#[repr(C)]
#[derive(CStruct)]
pub struct trans_waiting_for_lock {
    pub trans: *mut c::btree_trans,
    pub node_want: *mut c::btree_bkey_cached_common,
    #[c_bitfield]
    pub lock_want_bits: trans_waiting_for_lock_lock_want_bits,

    /* for iterating over held locks :*/
    pub level: u8,
    pub path_idx: c::btree_path_idx_t,
    pub waitlist_idx: u16,
    pub node_have: *mut c::btree_bkey_cached_common,

    /*
     * Conflicting waiters we found on the lock at (@path_idx, @level),
     * cached at snapshot time so iteration is stable against concurrent
     * wakeup activity. @waitlist_idx is the next entry to descend into.
     */
    #[c("DARRAY_PREALLOCATED(struct btree_trans *, 16) waitlist")]
    pub waitlist: DArray<*mut c::btree_trans, 16>,
}
c_default!(trans_waiting_for_lock);
impl trans_waiting_for_lock {
    pub fn lock_want(&self) -> u32 { let b = self.lock_want_bits; b.lock_want() }
    pub fn set_lock_want(&mut self, v: u32) { let mut b = self.lock_want_bits; b.set_lock_want(v); self.lock_want_bits = b; }
}

#[repr(C)]
#[derive(CStruct)]
pub struct lock_graph {
    pub g: [c::trans_waiting_for_lock; 8],
    pub nr: core::ffi::c_uint,
    pub printed_chain: bool,
}
c_default!(lock_graph);
