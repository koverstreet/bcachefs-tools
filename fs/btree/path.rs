// SPDX-License-Identifier: GPL-2.0

//! btree_path: the low level path to a btree node, which holds its locks.

#![allow(non_camel_case_types)]

use crate::btree::node::iter::KeyIter;
use crate::btree::node::BtreeNode;
use crate::cstructs::c as cs;
use crate::format::{Bpos, Btree};
use crate::types::c_default;
use cstruct_macros::{bitfield, c_typedef, CStruct};
use nestify::nest;

c_typedef! {
    pub type btree_path_idx_t = u16;
}

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct btree_path_level {
    pub b: *mut BtreeNode,
    pub iter: KeyIter,
    pub lock_seq: u32,
    #[cfg(CONFIG_BCACHEFS_LOCK_TIME_STATS)]
    pub lock_taken_time: u64,
}
c_default!(btree_path_level);

nest! {
    #[repr(C)]
    #[derive(Clone, Copy, CStruct)]
    pub struct btree_path {
        pub sorted_idx: cs::btree_path_idx_t,
        pub ref_: u8,
        pub intent_ref: u8,

        /* btree_iter_copy starts here: */
        pub pos: Bpos,

        #[c_bitfield]
        #>[bitfield(u16, ty = u32)]
        pub bits: pub struct btree_path_bits {
            #[bits(7)] pub btree_id: Btree,
            cached:           bool,
            preserve:         bool,
            /*
             * When true, failing to relock this path will cause the transaction to
             * restart:
             */
            should_be_locked: bool,
            level:            u3,
            locks_want:       u3,
        },
        pub nodes_locked: u8,

        pub l: [cs::btree_path_level; cs::BTREE_MAX_DEPTH as usize],
        /* C's TRACK_PATH_ALLOCATED, which btree/types_inline.h defines from
         * these: rustc sees only the configuration */
        #[cfg(any(CONFIG_BCACHEFS_LOCK_TIME_STATS, CONFIG_BCACHEFS_DEBUG))]
        pub ip_allocated: core::ffi::c_ulong,
    }
}
c_default!(btree_path);
