// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::fs::Fs;
use crate::data::reconcile::scan::OptChangeScans;
use crate::data::reconcile::work::Status;
use crate::locking::{Locked, SeqLock};
use crate::new_lock;
use crate::data::reconcile::work::StripesPending;
use crate::os::kthread::Daemon;
use core::sync::atomic::{AtomicBool, AtomicI32};
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
    pub scan_stats: c::MoveStats,

    /*
     * The scans the option change in progress has requested: the reconcile
     * thread doesn't start one of these - see data/reconcile/scan.rs
     */
    #[pin]
    #[init(new_lock!(OptChangeScans::new()))]
    pub(crate) opt_change_scans: Locked<c::spinlock_t, OptChangeScans>,

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
    /// Registered with the power supply class, which links it into its chain.
    #[cfg(CONFIG_POWER_SUPPLY)]
    #[pin]
    #[init(::pin_init::init_zeroed())]
    pub(crate) power_notifier:
        crate::util::place::Placed<crate::util::ffi::Opaque<c::notifier_block>,
                                   crate::at!(bch_fs_reconcile.power_notifier)>,
}

/// power_notifier's type, for its callback to cast to.
#[cfg(CONFIG_POWER_SUPPLY)]
pub(crate) type PowerNotifier = <crate::at!(bch_fs_reconcile.power_notifier) as crate::util::place::Place>::Type;

#[cfg(CONFIG_POWER_SUPPLY)]
unsafe impl ::pin_init::Zeroable for crate::util::ffi::Opaque<c::notifier_block> {}
