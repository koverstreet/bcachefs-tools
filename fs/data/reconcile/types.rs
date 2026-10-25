// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::data::reconcile::scan::OptChangeScans;
use crate::data::reconcile::work::Status;
use crate::locking::{Lock, SeqLock};
use crate::new_lock;
use crate::data::reconcile::work::StripesPending;
use crate::types::{RustOnly, rust_only};
use crate::util::kthread::Kthread;
use core::sync::atomic::{AtomicBool, AtomicI32};
use cstruct_macros::{CStruct, Init, TypeInfo};
use pin_init::pin_data;

#[pin_data]
#[repr(C)]
#[derive(CStruct, TypeInfo, Init)]
pub struct bch_fs_reconcile {
    /*
     * The thread, from start to stop - which doesn't mean it's running: it
     * also exits on its own if a pass fails, with thread_exit_ret its error
     */
    #[pin]
    #[init(RustOnly::new(new_lock!(None)))]
    pub(crate) thread: rust_only!(Lock<c::spinlock_t, Option<Kthread>>),
    #[pin]
    #[init(RustOnly::new(AtomicI32::new(0)))]
    pub(crate) thread_exit_ret: rust_only!(AtomicI32),

    /* What it's doing, for sysfs */
    #[pin]
    #[init(RustOnly::new(SeqLock::new(Status::new())))]
    pub(crate) status: rust_only!(SeqLock<Status>),
    pub work_stats: c::bch_move_stats,
    pub progress: c::progress_indicator,
    pub scan_stats: c::bch_move_stats,

    /*
     * The scans the option change in progress has requested: the reconcile
     * thread doesn't start one of these - see data/reconcile/scan.rs
     */
    #[pin]
    #[init(RustOnly::new(new_lock!(OptChangeScans::new())))]
    pub(crate) opt_change_scans: rust_only!(Lock<c::spinlock_t, OptChangeScans>),

    /*
     * Stripes whose repair failed in a way only a change elsewhere can fix
     * - no space, too few usable devices: the scan skips them, so reconcile
     * carries on with everything else and goes idle once they're all
     * that's left, instead of retrying them back to back. Stripes have no
     * on-disk pending state, as extents do; in memory, a remount retries
     * each once. The reconcile thread and its phys threads, one per
     * rotational device, park stripes all at once.
     */
    #[pin]
    #[init(RustOnly::new(::pin_init::init_zeroed()))]
    pub(crate) stripes_pending: rust_only!(StripesPending),

    /* Written by the power supply notifier */
    #[pin]
    #[init(RustOnly::new(AtomicBool::new(false)))]
    pub(crate) on_battery: rust_only!(AtomicBool),
    #[cfg(CONFIG_POWER_SUPPLY)]
    pub power_notifier: c::notifier_block,
}
