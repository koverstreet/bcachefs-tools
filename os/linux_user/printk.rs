// SPDX-License-Identifier: GPL-2.0

//! The log, for userspace: stderr - filesystem log messages, not command
//! output, so commands whose stdout is data (bcachefs list) aren't polluted
//! by recovery-pass chatter.

use core::ffi::CStr;
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
