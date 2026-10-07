// SPDX-License-Identifier: GPL-2.0

//! The data types of util/two_state_shared_lock_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::CStruct;

/*
 * Two-state lock - can be taken for add or block - both states are shared,
 * like read side of rwsem, but conflict with other state:
 */
#[repr(C)]
#[derive(Clone, Copy, CStruct)]
#[c_typedef]
pub struct two_state_lock_t {
    pub v: c::atomic_long_t,
    pub wait: c::wait_queue_head_t,
}
c_default!(two_state_lock_t);
