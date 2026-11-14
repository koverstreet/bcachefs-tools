// SPDX-License-Identifier: GPL-2.0

//! Rate limiting: not more than BURST events in every INTERVAL - lib/ratelimit.c's
//! ___ratelimit(), at its defaults, the only ones bcachefs uses.

use crate::os::kernel::{jiffies, time_after, HZ};
use crate::util::log::{log_no_fs, LogLevel};
use core::ffi::{c_ulong, CStr};
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering::{Acquire, Relaxed, Release}};

const INTERVAL: c_ulong = 5 * HZ;
const BURST: i32 = 10;

pub struct Ratelimit {
    /// Only ever tried: what finds it held goes by @n_left alone.
    lock:        AtomicBool,
    initialized: AtomicBool,
    /// In jiffies, under @lock
    begin:       AtomicUsize,
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
    pub fn allow(&self, func: &CStr) -> bool {
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

            if time_after(jiffies(), (self.begin.load(Relaxed) as c_ulong).wrapping_add(INTERVAL)) {
                /*
                 * Reset rs_n_left ASAP to reduce false positives
                 * in parallel calls, see above.
                 */
                self.n_left.store(BURST, Relaxed);
                self.begin.store(jiffies() as usize, Relaxed);

                let m = self.missed.swap(0, Relaxed);
                if m != 0 {
                    log_no_fs(LogLevel::WARN,
                              crate::message!("{}: {m} callbacks suppressed", func.to_str().unwrap_or("?")));
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
        self.n_left.store(0, Relaxed);
        self.missed.store(0, Relaxed);
        self.unlock();
    }
}
