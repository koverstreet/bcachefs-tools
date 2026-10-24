// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::locking::Lock;
use crate::new_lock;
use crate::types::{RustOnly, rust_only};
use crate::util::futex::EventCount;
use crate::util::kthread::Kthread;
use crate::util::projections::Volatile;
use crate::util::workqueue::Workqueue;
use cstruct_macros::{CStruct, Init, TypeInfo};
use pin_init::pin_data;

#[pin_data]
#[repr(C)]
#[derive(CStruct, TypeInfo, Init)]
pub struct bch_fs_copygc {
    /* The thread, from start to stop: data/copygc.rs */
    #[pin]
    #[init(RustOnly::new(new_lock!(None)))]
    pub(crate) thread: rust_only!(Lock<c::spinlock_t, Option<Kthread>>),
    pub write_point: c::write_point,
    /* The thread's, read unlocked - foreground.c's to_text, move.c: */
    pub wait_at: Volatile<i64>,
    pub wait: Volatile<i64>,
    pub running: Volatile<bool>,
    pub run_count: Volatile<u32>,
    /// Woken as a run ends: data/copygc.rs
    pub run_done: EventCount,

    /*
     * Devices over their fragmentation allowance, i.e. that copygc is trying
     * to free space on. Set by copygc_dev_list() each pass, read unlocked by
     * allocators that only need it to be roughly right.
     *
     * Not cleared when copygc is disabled: the devices are still full.
     */
    #[c("struct bch_devs_mask wants_space")]
    pub wants_space: c::AtomicDevsMask,

    /*
     * Devices getting full, on a looser threshold than wants_space above -
     * a superset of it, same lifetime and locking. Read by EC stripe reuse;
     * see EC_REUSE_FREE_THRESHOLD_PCT.
     */
    #[c("struct bch_devs_mask low_on_space")]
    pub low_on_space: c::AtomicDevsMask,

    /* Dedicated workqueue for btree updates: */
    #[c("struct workqueue_struct *wq")]
    pub wq: Workqueue,
}
