// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::{CStruct, c_rcu_array, TypeInfo};

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_disk_group_cpu {
    pub deleted: bool,
    pub parent: u16,
    pub label: [u8; c::BCH_SB_LABEL_SIZE as usize],
    pub devs: c::bch_devs_mask,
}

// SAFETY: plain data, a bitmap of devices among it; all zeroes is a group
unsafe impl crate::util::rcu::RcuElement for bch_disk_group_cpu {}

c_rcu_array! {
    /// c->disk_groups, published under RCU.
    pub type DiskGroups = crate::util::rcu::RcuArray<c::bch_disk_group_cpu>
        as struct bch_disk_groups_cpu { nr, entries };
}
