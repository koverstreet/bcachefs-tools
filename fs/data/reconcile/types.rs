// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::fs::Fs;
use crate::data::reconcile::scan::OptChangeScans;
use crate::data::reconcile::work::Status;
use crate::locking::{Locked, SeqLock, SpinLockIrq};
use crate::new_lock;
use crate::data::reconcile::work::StripesPending;
use crate::os::kthread::Daemon;
use crate::os::power_supply::PowerNotifier;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU64};
use cstruct_macros::{placed, MaybeZeroable, Init, TypeInfo};
use pin_init::pin_data;

#[placed(Fs.reconcile)]
#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, TypeInfo, Init)]
pub struct bch_fs_reconcile {
    /*
     * The thread, from start to stop - which doesn't mean it's running: it
     * also exits on its own if a pass fails, with thread_exit_ret its error
     */
    #[pin]
    #[init(Daemon::new())]
    pub(crate) thread: Daemon,
    #[pin]
    #[init(AtomicI32::new(0))]
    pub(crate) thread_exit_ret: AtomicI32,

    /* What it's doing, for sysfs */
    #[pin]
    #[init(SeqLock::new(Status::new()))]
    pub(crate) status: SeqLock<Status>,
    pub work_stats: c::MoveStats,
    pub progress: c::progress_indicator,
    /// The last phys phase's devices: rotational and online, those started
    /// - threads that did start - and those skipped for having no work
    #[init(AtomicU64::new(0))]
    pub(crate) phys_workers_considered: AtomicU64,
    #[init(AtomicU64::new(0))]
    pub(crate) phys_workers_started: AtomicU64,
    #[init(AtomicU64::new(0))]
    pub(crate) phys_workers_skipped_empty: AtomicU64,
    pub scan_stats: c::MoveStats,

    /*
     * The scans the option change in progress has requested: the reconcile
     * thread doesn't start one of these - see data/reconcile/scan.rs
     */
    #[pin]
    #[init(new_lock!(OptChangeScans::new()))]
    pub(crate) opt_change_scans: Locked<SpinLockIrq, OptChangeScans>,

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
    #[init(::pin_init::init_zeroed())]
    pub(crate) stripes_pending: StripesPending,

    /* Written by the power supply notifier */
    #[pin]
    #[init(AtomicBool::new(false))]
    pub(crate) on_battery: AtomicBool,
    #[pin]
    #[init(::pin_init::init_zeroed())]
    pub(crate) power_notifier: PowerNotifier<bch_fs_reconcile>,
}
