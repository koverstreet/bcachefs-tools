// SPDX-License-Identifier: GPL-2.0

//! The data types of data/copygc_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::CStruct;
use typeinfo_macros::TypeInfo;


#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_fs_copygc {
    #[c("struct task_struct __rcu *thread")]
    pub thread: *mut c::task_struct,
    pub write_point: c::write_point,
    pub wait_at: i64,
    pub wait: i64,
    pub running: bool,
    pub run_count: u32,
    pub kick_count: u32,
    pub running_wq: c::wait_queue_head_t,

    /*
     * Devices over their fragmentation allowance, i.e. that copygc is trying
     * to free space on. Set by copygc_dev_list() each pass, read unlocked by
     * allocators that only need it to be roughly right.
     *
     * Not cleared when copygc is disabled: the devices are still full.
     */
    pub wants_space: c::bch_devs_mask,

    /*
     * Devices getting full, on a looser threshold than wants_space above -
     * a superset of it, same lifetime and locking. Read by EC stripe reuse;
     * see EC_REUSE_FREE_THRESHOLD_PCT.
     */
    pub low_on_space: c::bch_devs_mask,

    /* Dedicated workqueue for btree updates: */
    pub wq: *mut c::workqueue_struct,
}
c_default!(bch_fs_copygc);
