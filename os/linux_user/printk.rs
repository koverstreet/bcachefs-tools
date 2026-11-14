// SPDX-License-Identifier: GPL-2.0

//! The log, for userspace: stderr - filesystem log messages, not command
//! output, so commands whose stdout is data (bcachefs list) aren't polluted
//! by recovery-pass chatter - and ratelimiting what goes to it.

use crate::c;
use crate::locking::rust_raw_spin_unlock;
use crate::os::kernel::{jiffies, time_after, HZ};
use core::ffi::{c_int, c_ulong, CStr};
use core::sync::atomic::{AtomicBool, AtomicI32, Ordering::{Acquire, Relaxed}};
use std::io::Write;

/*
 * The kernel's printk parses the KERN_SOH loglevel prefix from the
 * formatted output, so callers may pass it as a %s argument rather than
 * in the format string (bch2_print_string_as_lines(KERN_ERR, ...) does) -
 * format first, then strip, or the SOH and loglevel digit leak into the
 * output.
 */
fn skip_loglevel(mut s: &[u8]) -> &[u8] {
    while s.len() >= 2 && s[0] == 0x01 && s[1] != 0 {
        s = &s[2..];
    }
    s
}

/// @msg, which starts with its KERN_SOH and level, to the log.
pub fn printk(msg: &CStr) {
    let _ = std::io::stderr().write_all(skip_loglevel(msg.to_bytes()));
}

/// @prefix, @line and a newline to the log, as one message: @prefix starts
/// with its KERN_SOH and level.
pub fn printk_line(prefix: &[u8], line: &[u8]) {
    let msg = [prefix, line, b"\n"].concat();
    let _ = std::io::stderr().write_all(skip_loglevel(&msg));
}

/* ratelimit.h's */
const DEFAULT_RATELIMIT_INTERVAL: c_int = 5 * HZ as c_int;
const DEFAULT_RATELIMIT_BURST: c_int = 10;
const RATELIMIT_MSG_ON_RELEASE: u32 = 1 << 0;
const RATELIMIT_INITIALIZED: u32 = 1 << 1;

/// @rs as ratelimit_default_init() makes it.
///
/// # Safety
/// @rs is a ratelimit_state's memory, which nothing else reaches till this
/// returns.
pub unsafe fn ratelimit_state_init(rs: *mut c::ratelimit_state) {
    unsafe {
        rs.write_bytes(0, 1);
        (*rs).interval = DEFAULT_RATELIMIT_INTERVAL;
        (*rs).burst    = DEFAULT_RATELIMIT_BURST;
    }
}

/*
 * ___ratelimit - rate limiting
 *
 * This enforces a rate limit: not more than @rs->burst callbacks
 * in every @rs->interval
 *
 * Returns false where callbacks will be suppressed, true to go ahead.
 */
pub fn ratelimit(rs: &c::ratelimit_state, func: &CStr) -> bool {
    let rs = core::ptr::from_ref(rs).cast_mut();
    unsafe {
        let lock      = AtomicI32::from_ptr(&raw mut (*rs).lock.state);
        let n_left    = AtomicI32::from_ptr(&raw mut (*rs).rs_n_left.counter);
        let missed    = AtomicI32::from_ptr(&raw mut (*rs).missed.counter);
        let flags     = &raw mut (*rs).flags;
        let trylock   = || lock.compare_exchange(0, 1, Acquire, Relaxed).is_ok();
        let take_left = || n_left.load(Relaxed) > 0 && n_left.fetch_sub(1, Relaxed) > 0;

        let interval = (&raw const (*rs).interval).read_volatile();
        let burst    = (&raw const (*rs).burst).read_volatile();
        let ret;

        'nolock: {
            /*
             * Zero interval says never limit, otherwise, non-positive burst
             * says always limit.
             */
            if interval <= 0 || burst <= 0 {
                static WARNED: AtomicBool = AtomicBool::new(false);
                if (interval < 0 || burst < 0) && !WARNED.swap(true, Relaxed) {
                    eprintln!("WARNING: Negative interval ({interval}) or burst ({burst}): \
                               Uninitialized ratelimit_state structure?");
                }

                ret = interval == 0 || burst > 0;
                if flags.read_volatile() & RATELIMIT_INITIALIZED == 0 || (interval == 0 && burst == 0) ||
                    !trylock() {
                    break 'nolock;
                }

                /* Force re-initialization once re-enabled. */
                *flags &= !RATELIMIT_INITIALIZED;
                rust_raw_spin_unlock((&raw mut (*rs).lock).cast());
                break 'nolock;
            }

            /*
             * If we contend on this state's lock then just check if
             * the current burst is used or not. It might cause
             * false positive when we are past the interval and
             * the current lock owner is just about to reset it.
             */
            if !trylock() {
                ret = flags.read_volatile() & RATELIMIT_INITIALIZED != 0 && take_left();
                break 'nolock;
            }

            if *flags & RATELIMIT_INITIALIZED == 0 {
                (*rs).begin = jiffies();
                *flags |= RATELIMIT_INITIALIZED;
                n_left.store((*rs).burst, Relaxed);
            }

            if time_after(jiffies(), (*rs).begin.wrapping_add(interval as c_ulong)) {
                /*
                 * Reset rs_n_left ASAP to reduce false positives
                 * in parallel calls, see above.
                 */
                n_left.store((*rs).burst, Relaxed);
                (*rs).begin = jiffies();

                if *flags & RATELIMIT_MSG_ON_RELEASE == 0 {
                    let m = missed.swap(0, Relaxed);
                    if m != 0 {
                        eprintln!("{}: {m} callbacks suppressed", func.to_string_lossy());
                    }
                }
            }

            /* Note that the burst might be taken by a parallel call. */
            ret = take_left();

            rust_raw_spin_unlock((&raw mut (*rs).lock).cast());
        }

        if !ret {
            missed.fetch_add(1, Relaxed);
        }
        ret
    }
}
