// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{Le32, Le64};
use cstruct_macros::{c_enum, grown, CStruct, Plain, TypeInfo};

c_enum! {
    #[open]
    pub enum logged_ops_inums: u32 {
        LOGGED_OPS_INUM_logged_ops,
        LOGGED_OPS_INUM_inode_cursors,
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bch_logged_op_truncate {
    pub v: c::bch_val,
    pub subvol: Le32,
    pub pad: Le32,
    pub inum: Le64,
    pub new_i_size: Le64,
}

c_enum! {
    #[open]
    pub enum logged_op_finsert_state: u32 {
        LOGGED_OP_FINSERT_start,
        LOGGED_OP_FINSERT_shift_extents,
        LOGGED_OP_FINSERT_finish,
    }
}

#[grown(from = src_offset)]
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bch_logged_op_finsert {
    pub v: c::bch_val,
    pub state: u8,
    pub pad: [u8; 3],
    pub subvol: Le32,
    pub inum: Le64,
    pub dst_offset: Le64,
    pub src_offset: Le64,
    pub pos: Le64,
}

/*
 * Push one inode version's io options up to its ancestor snapshot versions, so
 * that data written before this branch existed sees them - see
 * bch2_inode_opt_propagate().
 *
 * The two snapshot ids have to be separate fields: "is this version off the
 * path we are propagating along" is asked relative to the origin, so folding
 * them together makes the origin itself look like a sibling one level up.
 *
 * No value or option id: the inode key at (@inum, @origin_snapshot) is the
 * source of truth, so a resumed op recomputes rather than replaying a value
 * that may have changed since.
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bch_logged_op_inode_opt_propagate {
    pub v: c::bch_val,
    pub inum: Le64,
    pub origin_snapshot: Le32,
    pub cursor_snapshot: Le32,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bch_logged_op_stripe_update {
    pub v: c::bch_val,
    pub old_idx: Le64,
    pub new_idx: Le64,
    pub old_blocks_nr: u8,
    pub old_block_map: [u8; 16],
    pub pad: [u8; 7],
}
