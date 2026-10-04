// SPDX-License-Identifier: GPL-2.0

//! Logging, as C's bch_log() family (bcachefs.h): a message is formatted
//! here, then handed to __bch2_print() whole, so it goes where C's messages go
//! - the filesystem's loglevel, a stdio redirect, the "previous loglevel" a
//! continuation line inherits - without any of that being reimplemented.
//!
//! In-kernel each message is prefixed with the filesystem's name, as
//! bch2_log_msg() does under BCACHEFS_LOG_PREFIX; userspace prints it bare.

use crate::c;
use crate::errcode::{bch_errcode, BchError};
use crate::fs::Fs;
use crate::util::Printbuf;
use core::fmt::{self, Write};

/// A printk loglevel: KERN_SOH followed by the level, as C's KERN_* strings.
#[derive(Clone, Copy)]
pub struct LogLevel(u8);

impl LogLevel {
    pub const ERR:    Self = LogLevel(b'3');
    pub const WARN:   Self = LogLevel(b'4');
    pub const NOTICE: Self = LogLevel(b'5');
    pub const INFO:   Self = LogLevel(b'6');
}

/// Print @args at @level: what the bch_err!() family expands to.
pub fn log(fs: &Fs, level: LogLevel, args: fmt::Arguments<'_>) {
    let mut buf = Printbuf::new();

    #[cfg(kernel)]
    {
        let name = unsafe { core::ffi::CStr::from_ptr(c::bch2_fs_name(fs.raw)) };
        let _ = write!(buf, "bcachefs ({}): ", name.to_str().unwrap_or("?"));
    }
    let _ = buf.write_fmt(args);
    buf.newline();

    // KERN_SOH, level, then the message as data: a '%' in it is text.
    let fmt = [0x01, level.0, b'%', b's', 0];
    unsafe {
        c::__bch2_print(fs.raw, fmt.as_ptr() as *const core::ffi::c_char,
                        buf.as_raw().buf);
    }
}

/// As C's should_print_err(): a restart isn't an error worth reporting.
pub fn should_print_err(err: &BchError) -> bool {
    !err.matches(bch_errcode::BCH_ERR_transaction_restart)
}

#[macro_export]
macro_rules! bch_err {
    ($fs:expr, $($arg:tt)*) => {
        $crate::util::log::log($fs, $crate::util::log::LogLevel::ERR, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! bch_warn {
    ($fs:expr, $($arg:tt)*) => {
        $crate::util::log::log($fs, $crate::util::log::LogLevel::WARN, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! bch_notice {
    ($fs:expr, $($arg:tt)*) => {
        $crate::util::log::log($fs, $crate::util::log::LogLevel::NOTICE, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! bch_info {
    ($fs:expr, $($arg:tt)*) => {
        $crate::util::log::log($fs, $crate::util::log::LogLevel::INFO, format_args!($($arg)*))
    };
}

/// The name of the function this is expanded in, as C's __func__ - the
/// function, not a closure inside it.
#[macro_export]
macro_rules! function_name {
    () => {{
        fn f() {}
        let mut name = core::any::type_name_of_val(&f);
        name = name.strip_suffix("::f").unwrap_or(name);
        while let Some(n) = name.strip_suffix("::{{closure}}") {
            name = n;
        }
        name.rsplit("::").next().unwrap_or(name)
    }};
}

/// As C's bch_err_fn(): report an error from @result, naming the function -
/// unless it's a restart, which isn't one.
#[macro_export]
macro_rules! bch_err_fn {
    ($fs:expr, $result:expr) => {
        if let Err(e) = &$result {
            if $crate::util::log::should_print_err(e) {
                $crate::bch_err!($fs, "{}(): error {}", $crate::function_name!(), e.msg());
            }
        }
    };
}
