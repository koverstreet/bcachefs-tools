// SPDX-License-Identifier: GPL-2.0

//! Logging, as C's bch_log() family (bcachefs.h): a message is formatted
//! here, then handed to __bch2_print() whole, so it goes where C's messages go -
//! the filesystem's loglevel, a stdio redirect, the "previous loglevel" a
//! continuation line inherits - without any of that being reimplemented.
//!
//! In-kernel each message is prefixed with the filesystem's name, as
//! bch2_log_msg() does under BCACHEFS_LOG_PREFIX; userspace prints it bare.
//!
//! What C shares - struct bch_log_msg, and the C this calls - is declared here
//! too: util/log_gen.h.

#![allow(non_camel_case_types)]

use crate::c;
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::types::c_default;
use crate::util::ffi::c_char;
use crate::util::{Printbuf, ToText};
use core::cell::UnsafeCell;
use core::ffi::{c_int, c_uint, CStr};
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, Ordering};
use cstruct_macros::{CStruct, c_enum, c_extern, TypeInfo};

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_log_msg {
    pub c: *mut c::bch_fs,
    pub loglevel: u8,
    pub m: c::printbuf,
}
c_default!(bch_log_msg);

c_enum! {
    #[closed]
    pub enum kern_loglevels: u32 {
        LOGLEVEL_emerg = 0,
        LOGLEVEL_alert = 1,
        LOGLEVEL_crit = 2,
        LOGLEVEL_err = 3,
        LOGLEVEL_warning = 4,
        LOGLEVEL_notice = 5,
        LOGLEVEL_info = 6,
        LOGLEVEL_debug = 7,
    }
}

c_extern! {
    #[c_printf(2, 3)]
    pub fn __bch2_print(c: *mut c::bch_fs, fmt: *const c_char, ...);
    pub fn bch2_ratelimit_state_init(rs: *mut c::ratelimit_state);
    pub fn bch2_ratelimit_suppress(c: *mut c::bch_fs, rs: *mut c::ratelimit_state,
                                   func: *const c_char) -> bool;
    pub fn bch2_rust_warn(file: *const c_char, line: c_uint);
    pub fn bch2_print_str_loglevel(c: *mut c::bch_fs, loglevel: c_int, str: *const c_char);
    pub fn __bch2_log_msg_start(fs_or_dev_name: *const c_char, out: *mut c::printbuf);
}

/// A printk loglevel: KERN_SOH followed by the level, as C's KERN_* strings.
#[derive(Clone, Copy)]
pub struct LogLevel(u8);

impl LogLevel {
    pub const ERR:    Self = LogLevel(b'3');
    pub const WARN:   Self = LogLevel(b'4');
    pub const NOTICE: Self = LogLevel(b'5');
    pub const INFO:   Self = LogLevel(b'6');
    pub const DEBUG:  Self = LogLevel(b'7');

    /// As C's LOGLEVEL_* numbers.
    fn as_int(self) -> i32 {
        (self.0 - b'0') as i32
    }
}

/// Print @msg at @level: what the bch_err!() family expands to.
pub fn log(fs: &Fs, level: LogLevel, msg: &dyn ToText) {
    let mut buf = Printbuf::new();

    #[cfg(kernel)]
    {
        write!(buf, "bcachefs ({}): ", fs.name().to_str().unwrap_or("?"));
    }
    msg.to_text(&mut buf);
    buf.newline();

    // KERN_SOH, level, then the message as data: a '%' in it is text.
    let fmt = [0x01, level.0, b'%', b's', 0];
    unsafe {
        __bch2_print(fs.as_ptr(), fmt.as_ptr() as *const c_char, buf.as_raw().buf);
    }
}

/// Print @msg at @level as it stands, one line at a time with no prefix -
/// C's bch2_print_str().
pub fn print_str(fs: &Fs, level: LogLevel, msg: &mut Printbuf) {
    let buf = msg.as_raw().buf;
    if !buf.is_null() {
        unsafe { bch2_print_str_loglevel(fs.as_ptr(), level.as_int(), buf) };
    }
}

/// A message built up in pieces and printed when dropped - C's
/// CLASS(bch_log_msg). Continuation lines are indented under the first and
/// raw `\t`/`\r` columns aligned; it goes out whole, through
/// bch2_print_str_loglevel() as C's does, unless suppressed - by
/// count_fsck_err(), when it's a repeat or being ratelimited.
pub struct LogMsg<'a> {
    fs:    &'a Fs,
    level: LogLevel,
    msg:   Printbuf,
}

impl<'a> LogMsg<'a> {
    /// At err.
    pub fn new(fs: &'a Fs) -> Self {
        Self::with_level(fs, LogLevel::ERR)
    }

    pub fn with_level(fs: &'a Fs, level: LogLevel) -> Self {
        let mut msg = Printbuf::new();
        // The fs name prefix, in-kernel, and the continuation indent:
        unsafe { __bch2_log_msg_start(fs.name().as_ptr().cast(), msg.as_raw()) };
        LogMsg { fs, level, msg }
    }

    /// For a message whose level depends on how things turned out.
    pub fn set_level(&mut self, level: LogLevel) {
        self.level = level;
    }
}

impl core::ops::Deref for LogMsg<'_> {
    type Target = Printbuf;
    fn deref(&self) -> &Printbuf { &self.msg }
}

impl core::ops::DerefMut for LogMsg<'_> {
    fn deref_mut(&mut self) -> &mut Printbuf { &mut self.msg }
}

impl Drop for LogMsg<'_> {
    fn drop(&mut self) {
        if self.msg.is_suppressed() {
            return;
        }

        self.msg.tabstop_align();
        let buf = self.msg.as_raw().buf;
        if !buf.is_null() {
            unsafe { bch2_print_str_loglevel(self.fs.as_ptr(), self.level.as_int(), buf) };
        }
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

impl Default for Ratelimit {
    fn default() -> Self {
        Self::new()
    }
}

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
            unsafe { bch2_ratelimit_state_init(state) };
            self.ready.store(true, Ordering::Release);
        }

        unsafe { bch2_ratelimit_suppress(fs.as_ptr(), state, func.as_ptr().cast()) }
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
            $crate::util::log::log(fs, $level, $crate::message!($($arg)*));
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

#[macro_export]
macro_rules! bch_verbose_ratelimited {
    ($fs:expr, $($arg:tt)*) => {
        $crate::bch_log_ratelimited!($fs, $crate::util::log::LogLevel::DEBUG, $($arg)*)
    };
}

/// As C's should_print_err(): a restart isn't an error worth reporting.
pub fn should_print_err(err: &BchError) -> bool {
    !crate::btree::iter::is_restart(err)
}

#[macro_export]
macro_rules! bch_err {
    ($fs:expr, $($arg:tt)*) => {
        $crate::util::log::log($fs, $crate::util::log::LogLevel::ERR, $crate::message!($($arg)*))
    };
}

#[macro_export]
macro_rules! bch_warn {
    ($fs:expr, $($arg:tt)*) => {
        $crate::util::log::log($fs, $crate::util::log::LogLevel::WARN, $crate::message!($($arg)*))
    };
}

#[macro_export]
macro_rules! bch_notice {
    ($fs:expr, $($arg:tt)*) => {
        $crate::util::log::log($fs, $crate::util::log::LogLevel::NOTICE, $crate::message!($($arg)*))
    };
}

#[macro_export]
macro_rules! bch_info {
    ($fs:expr, $($arg:tt)*) => {
        $crate::util::log::log($fs, $crate::util::log::LogLevel::INFO, $crate::message!($($arg)*))
    };
}

/// As C's bch_verbose(): KERN_DEBUG.
#[macro_export]
macro_rules! bch_verbose {
    ($fs:expr, $($arg:tt)*) => {
        $crate::util::log::log($fs, $crate::util::log::LogLevel::DEBUG, $crate::message!($($arg)*))
    };
}

/// As C's WARN_ON(): the kernel's warning in-kernel - through C, not the
/// kernel crate's warn_on!(), see bch2_rust_warn() - and what the userspace
/// shim's WARN_ON() prints otherwise. Evaluates to @cond.
#[macro_export]
macro_rules! warn_on {
    ($cond:expr) => {{
        let cond: bool = $cond;
        #[cfg(kernel)]
        if cond {
            // SAFETY: a NUL-terminated string literal.
            unsafe {
                $crate::util::log::bch2_rust_warn(
                    concat!(file!(), "\0").as_ptr().cast(),
                    line!(),
                )
            };
        }
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

impl Default for CFnName {
    fn default() -> Self {
        Self::new()
    }
}

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
                                 $crate::message!($($arg)*), e.msg());
            }
        }
        result
    }};
}
