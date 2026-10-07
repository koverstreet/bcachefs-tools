// SPDX-License-Identifier: GPL-2.0

//! The data types of btree/locking_defs.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::c_enum;

/* path lock state */

/* matches six lock types */
c_enum! {
    #[closed]
    pub enum btree_node_locked_type: i32 {
        BTREE_NODE_UNLOCKED = -1,
        BTREE_NODE_READ_LOCKED = c::SIX_LOCK_read as i32,
        BTREE_NODE_INTENT_LOCKED = c::SIX_LOCK_intent as i32,
        BTREE_NODE_WRITE_LOCKED = c::SIX_LOCK_write as i32,
    }
}
