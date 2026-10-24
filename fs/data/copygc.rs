// SPDX-License-Identifier: GPL-2.0

//! copygc.c's, for Rust and through Rust: kicking copygc, and waiting on its
//! runs. A run ends with running cleared, run_count bumped, and run_done
//! woken - an EventCount, where C had a waitqueue: what waits for a run is in
//! Rust, its condition with it, and C calls in.
//!
//! running and run_count are plain fields the copygc thread writes, read here
//! unlocked: read_volatile(), as C's READ_ONCE() would, until they're typed.
//! The wake's release orders them for a waiter, whose snapshot acquires.

use crate::c;
use crate::fs::Fs;
use crate::util::kthread::Kthread;
use crate::util::rcu::read_lock;
use crate::util::sleep::Sleep;
use cstruct_macros::c_export;

impl c::bch_fs_copygc {
    fn running(&self) -> bool {
        // SAFETY: a bool, written whole
        unsafe { core::ptr::read_volatile(&raw const self.running) }
    }

    /// The runs it's finished.
    pub fn run_count(&self) -> u32 {
        // SAFETY: a u32, written whole
        unsafe { core::ptr::read_volatile(&raw const self.run_count) }
    }
}

impl Fs {
    /// Kick copygc into a run: bch2_copygc_wakeup(). The kick count's bumped
    /// as C's ++ is, racy - a kick lost to a concurrent one is still a kick,
    /// and the thread's woken either way.
    pub fn copygc_wakeup(&self) {
        // SAFETY: the copygc fields, through the raw pointer, as C writes
        // them; the thread pointer RCU's, read under it
        unsafe {
            let kick = &raw mut (*self.as_ptr()).copygc.kick_count;
            core::ptr::write_volatile(kick, core::ptr::read_volatile(kick).wrapping_add(1));

            let _rcu = read_lock();
            let thread = core::ptr::read_volatile(&raw const (*self.as_ptr()).copygc.thread);
            if !thread.is_null() {
                c::wake_up_process(thread);
            }
        }
    }

    /// Until a copygc run ends after @run_count, or this kthread's told to
    /// stop: the run count then. Freezable, as copygc freezes in place; idle,
    /// as a run can take longer than the hung task timeout.
    pub fn copygc_wait_run(&self, run_count: u32) -> u32 {
        let cg = &self.copygc;
        let Ok(()) = cg.run_done.wait_until(Sleep::IDLE.freezable(),
                                            || (cg.run_count() != run_count || Kthread::should_stop()).then_some(()));
        cg.run_count()
    }
}

/// A copygc run's ended - running cleared, run_count bumped: its waiters
/// look again.
#[c_export]
pub fn bch2_copygc_run_done(c: &Fs) {
    c.copygc.run_done.wake();
}

/// Until copygc isn't running, or this kthread's told to stop: for a moving
/// context that waits on copygc, bch2_move_ratelimit()'s. Freezable - copygc
/// freezes in place, mid run - and interruptible, as wait_event_freezable().
#[c_export]
pub fn bch2_copygc_wait_not_running(c: &Fs) {
    let cg = &c.copygc;
    let _ = cg.run_done.wait_until(Sleep::INTERRUPTIBLE.freezable(),
                                   || (!cg.running() || Kthread::should_stop()).then_some(()));
}
