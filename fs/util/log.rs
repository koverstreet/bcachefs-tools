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
use core::cell::UnsafeCell;
use core::ffi::CStr;
use core::fmt::{self, Write};
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, Ordering};

/// A printk loglevel: KERN_SOH followed by the level, as C's KERN_* strings.
#[derive(Clone, Copy)]
pub struct LogLevel(u8);

impl LogLevel {
    pub const ERR:    Self = LogLevel(b'3');
    pub const WARN:   Self = LogLevel(b'4');
    pub const NOTICE: Self = LogLevel(b'5');
    pub const INFO:   Self = LogLevel(b'6');
    pub const DEBUG:  Self = LogLevel(b'7');
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

/// One call site's ratelimit state, for the _ratelimited log macros: as the
/// static DEFINE_RATELIMIT_STATE() in C's bch2_ratelimit(). The state holds a
/// spinlock that has to be initialized at runtime, so it's set up on first use.
#[doc(hidden)]
pub struct Ratelimit {
    claim: AtomicBool,
    ready: AtomicBool,
    state: UnsafeCell<MaybeUninit<c::ratelimit_state>>,
}

// state is initialized once, by whoever claims it, before ready is published;
// after that only ___ratelimit() touches it, under its lock.
unsafe impl Sync for Ratelimit {}

impl Ratelimit {
    pub const fn new() -> Self {
        Ratelimit {
            claim: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            state: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    /// Whether to drop a message from @func: as bch2_ratelimit(), which only
    /// ratelimits with the ratelimit_errors option. A message that races the
    /// first one's setup of the state isn't ratelimited.
    pub fn suppress(&self, fs: &Fs, func: &CStr) -> bool {
        let state = unsafe { (*self.state.get()).as_mut_ptr() };

        if !self.ready.load(Ordering::Acquire) {
            if self.claim.swap(true, Ordering::AcqRel) {
                return false;
            }
            unsafe { c::bch2_ratelimit_state_init(state) };
            self.ready.store(true, Ordering::Release);
        }

        unsafe { c::bch2_ratelimit_suppress(fs.raw, state, func.as_ptr()) }
    }
}

/// As C's bch_log_ratelimited(): what the bch_err_ratelimited!() family
/// expands to, with ratelimit state per call site.
#[macro_export]
macro_rules! bch_log_ratelimited {
    ($fs:expr, $level:expr, $($arg:tt)*) => {{
        static RATELIMIT: $crate::util::log::Ratelimit = $crate::util::log::Ratelimit::new();
        let fs: &$crate::fs::Fs = $fs;
        if !RATELIMIT.suppress(fs, $crate::c_function_name!()) {
            $crate::util::log::log(fs, $level, format_args!($($arg)*));
        }
    }};
}

#[macro_export]
macro_rules! bch_err_ratelimited {
    ($fs:expr, $($arg:tt)*) => {
        $crate::bch_log_ratelimited!($fs, $crate::util::log::LogLevel::ERR, $($arg)*)
    };
}

#[macro_export]
macro_rules! bch_warn_ratelimited {
    ($fs:expr, $($arg:tt)*) => {
        $crate::bch_log_ratelimited!($fs, $crate::util::log::LogLevel::WARN, $($arg)*)
    };
}

#[macro_export]
macro_rules! bch_info_ratelimited {
    ($fs:expr, $($arg:tt)*) => {
        $crate::bch_log_ratelimited!($fs, $crate::util::log::LogLevel::INFO, $($arg)*)
    };
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

/// As C's bch_verbose(): KERN_DEBUG.
#[macro_export]
macro_rules! bch_verbose {
    ($fs:expr, $($arg:tt)*) => {
        $crate::util::log::log($fs, $crate::util::log::LogLevel::DEBUG, format_args!($($arg)*))
    };
}

/// As C's WARN_ON(): the kernel's warning in-kernel, and what the userspace
/// shim's WARN_ON() prints otherwise. Evaluates to @cond.
#[macro_export]
macro_rules! warn_on {
    ($cond:expr) => {{
        let cond: bool = $cond;
        #[cfg(kernel)]
        kernel::warn_on!(cond);
        #[cfg(not(kernel))]
        if cond {
            std::eprintln!("WARNING at {}:{}", file!(), line!());
        }
        cond
    }};
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

/// As function_name!(), but as a C string that lives forever, for C that
/// takes __func__.
#[macro_export]
macro_rules! c_function_name {
    () => {{
        static NAME: $crate::util::log::CFnName = $crate::util::log::CFnName::new();
        NAME.get($crate::function_name!())
    }};
}

/// One call site's function name, for c_function_name!(): Rust only has it at
/// runtime and unterminated, so it's copied into a static buffer on first use.
#[doc(hidden)]
pub struct CFnName {
    claim: AtomicBool,
    ready: AtomicBool,
    buf:   UnsafeCell<[u8; 64]>,
}

// buf is written once, by whoever claims it, before ready is published, and
// only read after that.
unsafe impl Sync for CFnName {}

impl CFnName {
    pub const fn new() -> Self {
        CFnName {
            claim: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            buf:   UnsafeCell::new([0; 64]),
        }
    }

    /// @name, NUL-terminated and truncated to fit: "(unknown)" while another
    /// thread is filling it in.
    pub fn get(&self, name: &str) -> &CStr {
        if !self.ready.load(Ordering::Acquire) {
            if self.claim.swap(true, Ordering::AcqRel) {
                return c"(unknown)";
            }

            let buf = unsafe { &mut *self.buf.get() };
            let n = name.len().min(buf.len() - 1);
            buf[..n].copy_from_slice(&name.as_bytes()[..n]);
            buf[n] = 0;
            self.ready.store(true, Ordering::Release);
        }

        let buf = unsafe { &*self.buf.get() };
        CStr::from_bytes_until_nul(buf).expect("buf is NUL-terminated")
    }
}

/// As C's bch_err_fn(): report an error from @result, naming the function -
/// unless it's a restart, which isn't one. Evaluates to @result, so it can
/// wrap the call: `bch_err_fn!(fs, f())?`.
#[macro_export]
macro_rules! bch_err_fn {
    ($fs:expr, $result:expr) => {{
        let result = $result;
        if let Err(e) = &result {
            let e: $crate::errcode::BchError = (*e).into();
            if $crate::util::log::should_print_err(&e) {
                $crate::bch_err!($fs, "{}(): error {}", $crate::function_name!(), e.msg());
            }
        }
        result
    }};
}

/// As C's bch_err_msg(): bch_err_fn!(), saying what we were doing. Evaluates
/// to @result, as bch_err_fn!() does.
#[macro_export]
macro_rules! bch_err_msg {
    ($fs:expr, $result:expr, $($arg:tt)*) => {{
        let result = $result;
        if let Err(e) = &result {
            let e: $crate::errcode::BchError = (*e).into();
            if $crate::util::log::should_print_err(&e) {
                $crate::bch_err!($fs, "{}(): error {} {}", $crate::function_name!(),
                                 format_args!($($arg)*), e.msg());
            }
        }
        result
    }};
}
