// SPDX-License-Identifier: GPL-2.0

//! Rate limiting: not more than BURST events in every interval - lib/ratelimit.c's
//! ___ratelimit(), at its defaults, the only ones bcachefs uses - but backing
//! off: while events keep being suppressed, each interval is twice the last,
//! up to INTERVAL_MAX, so a flood that doesn't stop doesn't fill the log at a
//! steady BURST per INTERVAL. One quiet interval and it's INTERVAL again.

use crate::os::kernel::{jiffies, time_after, HZ};
use crate::util::log::Log;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering::{Acquire, Relaxed, Release}};

const INTERVAL: usize = 5 * HZ;
const INTERVAL_MAX: usize = 5 * 60 * HZ;
const BURST: i32 = 10;

pub struct Ratelimit {
    /// Only ever tried: what finds it held goes by @n_left alone.
    lock:        AtomicBool,
    initialized: AtomicBool,
    /// In jiffies, under @lock
    begin:       AtomicUsize,
    interval:    AtomicUsize,
    n_left:      AtomicI32,
    missed:      AtomicI32,
}

impl Default for Ratelimit {
    fn default() -> Self {
        Self::new()
    }
}

impl Ratelimit {
    pub const fn new() -> Self {
        Ratelimit {
            lock:        AtomicBool::new(false),
            initialized: AtomicBool::new(false),
            begin:       AtomicUsize::new(0),
            interval:    AtomicUsize::new(INTERVAL),
            n_left:      AtomicI32::new(0),
            missed:      AtomicI32::new(0),
        }
    }

    fn trylock(&self) -> bool {
        self.lock.compare_exchange(false, true, Acquire, Relaxed).is_ok()
    }

    fn unlock(&self) {
        self.lock.store(false, Release)
    }

    fn take_left(&self) -> bool {
        self.n_left.load(Relaxed) > 0 && self.n_left.fetch_sub(1, Relaxed) > 0
    }

    /// Whether an event from @func may go ahead. The first to after events
    /// were suppressed logs how many.
    pub fn allow(&self, func: &dyn core::fmt::Display) -> bool {
        let ret = 'nolock: {
            /*
             * If we contend on this state's lock then just check if
             * the current burst is used or not. It might cause
             * false positive when we are past the interval and
             * the current lock owner is just about to reset it.
             */
            if !self.trylock() {
                break 'nolock self.initialized.load(Relaxed) && self.take_left();
            }

            if !self.initialized.load(Relaxed) {
                self.begin.store(jiffies() as usize, Relaxed);
                self.initialized.store(true, Relaxed);
                self.n_left.store(BURST, Relaxed);
            }

            let now = jiffies();
            let begin = self.begin.load(Relaxed);
            let interval = self.interval.load(Relaxed);
            if time_after(now, begin.wrapping_add(interval)) {
                /*
                 * Reset rs_n_left ASAP to reduce false positives
                 * in parallel calls, see above.
                 */
                self.n_left.store(BURST, Relaxed);
                self.begin.store(now as usize, Relaxed);

                let m = self.missed.swap(0, Relaxed);
                // still flooding: events suppressed, and this one within an
                // interval of the last's end
                let flooding = m != 0 && !time_after(now, begin.wrapping_add(2 * interval));
                let next = if flooding { (2 * interval).min(INTERVAL_MAX) } else { INTERVAL };
                self.interval.store(next as usize, Relaxed);

                if m != 0 {
                    Log::NO_FS.warn(crate::message!("{func}: {m} callbacks suppressed"));
                }
            }

            /* Note that the burst might be taken by a parallel call. */
            let ret = self.take_left();
            self.unlock();
            ret
        };

        if !ret {
            self.missed.fetch_add(1, Relaxed);
        }
        ret
    }

    /// Started over, as if nothing had been ratelimited.
    pub fn reset(&self) {
        while !self.trylock() {
            core::hint::spin_loop();
        }
        self.initialized.store(false, Relaxed);
        self.begin.store(0, Relaxed);
        self.interval.store(INTERVAL, Relaxed);
        self.n_left.store(0, Relaxed);
        self.missed.store(0, Relaxed);
        self.unlock();
    }
}
