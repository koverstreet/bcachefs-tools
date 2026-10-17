// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::CStruct;

/*
 * What changing an inode's options leaves for after the commit: an inode
 * option change has to reach existing data (a reconcile scan) and older
 * snapshots of the inode (the propagate logged op, finished after commit).
 */
#[repr(C)]
#[derive(Default, CStruct)]
pub struct inode_opt_change {
    pub reconcile_changed: bool,
    pub propagate: c::bkey_i_logged_op_inode_opt_propagate,
}
