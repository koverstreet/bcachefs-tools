// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{DArray, c_default};
use core::ffi::c_uint;
use cstruct_macros::{bitfield, c_extern, CStruct};
use nestify::nest;

c_extern! {
    pub fn rust_bch2_inode_shard_idx(c: *mut c::bch_fs) -> u64;
}

/* State used for the cycle detector */
/*
 * @trans wants to lock @b with type @type
 */
nest! {
    #[repr(C)]
    #[derive(CStruct)]
    pub struct trans_waiting_for_lock {
        pub trans: *mut c::btree_trans,
        pub node_want: *mut c::btree_bkey_cached_common,
        #[c_bitfield]
        #>[bitfield(u8)]
        pub bits: pub struct trans_waiting_for_lock_bits {
            #[bits(8)]
            pub lock_want: c::six_lock_type,
        },

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
        pub waitlist: DArray<*mut c::btree_trans, 16>,
    }
}
c_default!(trans_waiting_for_lock);

#[repr(C)]
#[derive(CStruct)]
pub struct lock_graph {
    pub g: [c::trans_waiting_for_lock; 8],
    pub nr: c_uint,
    pub printed_chain: bool,
}
c_default!(lock_graph);
