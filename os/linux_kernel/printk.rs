// SPDX-License-Identifier: GPL-2.0

//! The log: printk(), and ratelimiting what goes to it.

use crate::c;
use core::ffi::{c_int, CStr};

/// @msg, which starts with its KERN_SOH and level, to the log as data: a '%'
/// in it is text.
pub fn printk(msg: &CStr) {
    unsafe { c::_printk(c"%s".as_ptr().cast(), msg.as_ptr()) };
}

/// @prefix, @line and a newline to the log, as one message: @prefix starts
/// with its KERN_SOH and level.
pub fn printk_line(prefix: &[u8], line: &[u8]) {
    let (p, l) = (prefix.len() as c_int, line.len() as c_int);
    unsafe { c::_printk(c"%.*s%.*s\n".as_ptr().cast(), p, prefix.as_ptr(), l, line.as_ptr()) };
}

/// Whether @rs lets a message from @func through: ___ratelimit().
pub fn ratelimit(rs: &c::ratelimit_state, func: &CStr) -> bool {
    unsafe { c::___ratelimit(core::ptr::from_ref(rs).cast_mut(), func.as_ptr().cast()) != 0 }
}

/// @rs as ratelimit_default_init() makes it.
///
/// # Safety
/// @rs is a ratelimit_state's memory, which nothing else reaches till this
/// returns.
pub unsafe fn ratelimit_state_init(rs: *mut c::ratelimit_state) {
    unsafe { c::bch2_ratelimit_state_init(rs) }
}
