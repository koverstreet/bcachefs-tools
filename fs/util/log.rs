// SPDX-License-Identifier: GPL-2.0

//! Logging, as C's bch_log() family was: a message goes where the filesystem
//! sends it - past its loglevel, to a stdio redirect, at the "previous
//! loglevel" a continuation line inherits.
//!
//! In-kernel each message is prefixed with the filesystem's name; userspace
//! prints it bare.
//!
//! What C shares - struct bch_log_msg, and the C this calls - is declared here
//! too: util/log_gen.h.

#![allow(non_camel_case_types)]

use crate::fs::Dev;
use crate::c;
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::types::c_default;
use crate::util::ratelimit::Ratelimit;
use crate::util::{Printbuf, ToText};
use core::ffi::CStr;
use core::sync::atomic::Ordering;
use cstruct_macros::{MaybeZeroable, bch_enum, TypeInfo};

#[repr(C)]
#[derive(MaybeZeroable, TypeInfo)]
pub struct bch_log_msg {
    pub c: *mut Fs,
    pub loglevel: u8,
    pub m: Printbuf,
}
c_default!(bch_log_msg);

#[bch_enum(closed)]
#[repr(u32)]
pub enum KernLoglevel {
    LOGLEVEL_emerg = 0,
    LOGLEVEL_alert = 1,
    LOGLEVEL_crit = 2,
    LOGLEVEL_err = 3,
    LOGLEVEL_warning = 4,
    LOGLEVEL_notice = 5,
    LOGLEVEL_info = 6,
    LOGLEVEL_debug = 7,
}

pub use crate::c::bch2_rust_warn;

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
    fn num(self) -> u8 {
        self.0 - b'0'
    }

    /// As C's KERN_* strings
    pub fn prefix(self) -> [u8; 2] {
        [0x01, self.0]
    }
}

/// Print @msg at @level: what the bch_err!() family expands to.
pub fn log(fs: &Fs, level: LogLevel, msg: &dyn ToText) {
    print(fs, level.0, true, msg, true)
}

/// @msg at @level, of no filesystem - there isn't one yet: pr_err() and the
/// rest of printk().
pub fn log_no_fs(level: LogLevel, msg: &dyn ToText) {
    printk(level, &crate::util::printbuf::Msg(|out: &mut Printbuf| {
        msg.to_text(out);
        out.newline();
    }))
}

/// @msg at @level, as it stands, where @opts says: its stdio redirect,
/// in-kernel, if it has one - a fsck through the ioctl's - else printk().
/// Of no filesystem, before there's one: bch2_print_opts().
pub fn log_opts(opts: &c::bch_opts, level: LogLevel, msg: &dyn ToText) {
    #[cfg(kernel)]
    if opts.stdio != 0 {
        let mut buf = Printbuf::new();
        msg.to_text(&mut buf);
        // SAFETY: @opts' stdio is the fsck thread's redirect, live for as
        // long as its opts are
        let stdio = unsafe { &*(opts.stdio as usize as *const crate::os::thread_with_file::StdioRedirect) };
        let _ = stdio.write(true, buf.as_bytes());
        return;
    }
    #[cfg(not(kernel))]
    let _ = opts;
    printk(level, msg)
}

/// @msg at @level, as it stands.
fn printk(level: LogLevel, msg: &dyn ToText) {
    let mut buf = Printbuf::new();
    buf.write_bytes(&[0x01, level.0]);
    msg.to_text(&mut buf);
    printk_buf(&buf)
}

/// @buf, which starts with its KERN_SOH and level, to the log as data: a '%'
/// in it is text.
fn printk_buf(buf: &Printbuf) {
    crate::os::printk::printk(buf.as_c_str())
}

/// log(), without the newline: a line for log_cont() to finish - C's
/// bch2_print(c, KERN_INFO bch2_log_msg(c, ...)).
pub fn log_start(fs: &Fs, level: LogLevel, msg: &dyn ToText) {
    print(fs, level.0, true, msg, false)
}

/// The rest of the line log_start() began: KERN_CONT.
pub fn log_cont(fs: &Fs, msg: &dyn ToText) {
    print(fs, b'c', false, msg, true)
}

/// @msg at printk level @level, after the filesystem's name if @prefix, and
/// a newline if @newline.
fn print(fs: &Fs, level: u8, prefix: bool, msg: &dyn ToText, newline: bool) {
    // A level sets the one continuations - KERN_CONT, anything not a digit -
    // print at.
    let loglevel = if level.is_ascii_digit() {
        let l = u32::from(level - b'0');
        fs.prev_loglevel.store(l, Ordering::Relaxed);
        l
    } else {
        fs.prev_loglevel.load(Ordering::Relaxed)
    };
    if loglevel > max_loglevel(fs) {
        return;
    }

    let mut buf = Printbuf::new();
    buf.write_bytes(&[0x01, level]);

    #[cfg(kernel)]
    if prefix {
        write!(buf, "bcachefs ({}): ", fs.name().to_str().unwrap_or("?"));
    }
    #[cfg(not(kernel))]
    let _ = prefix;
    msg.to_text(&mut buf);
    if newline {
        buf.newline();
    }

    #[cfg(kernel)]
    if let Some(stdio) = stdio_redirect(fs, false) {
        let _ = stdio.write(true, &buf.as_bytes()[2..]);
        return;
    }
    printk_buf(&buf)
}

/// The filesystem's loglevel: the most verbose level printed.
fn max_loglevel(fs: &Fs) -> u32 {
    match fs.loglevel.load(Ordering::Relaxed) {
        0 if fs.opts().verbose() => 7,
        0 => 6,
        l => l,
    }
}

/// @msg at @level after device @ca's name, as C's bch_dev_log() - in
/// userspace too, unlike the filesystem's.
pub fn log_dev(ca: &Dev, level: LogLevel, msg: &dyn ToText) {
    let msg = crate::util::printbuf::Msg(|out: &mut Printbuf| {
        #[cfg(kernel)]
        write!(out, "bcachefs ({ca}): ");
        #[cfg(not(kernel))]
        write!(out, "{ca}: ");
        msg.to_text(out);
    });
    print(ca.fs(), level.0, false, &msg, true)
}

/// Print @msg at @level as it stands, one line at a time with no prefix.
pub fn print_str(fs: &Fs, level: LogLevel, msg: &mut Printbuf) {
    print_bytes(fs, Some(level.num()), msg.as_bytes())
}

// ── A message printed whole ─────────────────────────────────────────────
//
// A message is printed at a loglevel - or, given none, the previous one's: a
// continuation - if the filesystem's loglevel lets it through, and goes to
// the filesystem's stdio redirect if it has one: a fsck through the ioctl, or
// a mount that asked for one. Otherwise it's printk()ed a chunk at a time,
// whole lines up to printk's limit, with the console held across them if it's
// free, so a long message isn't interleaved with another's.

/// The most printk() takes at once.
const PRINTK_LIMIT: usize = 1024;

/// The filesystem's redirect, if it's to get this message - @user: one for
/// the person running the fsck, which a mount that only wants those gets -
/// and if this is the task it's for.
#[cfg(kernel)]
pub(crate) fn stdio_redirect(fs: &Fs, user: bool) -> Option<&crate::os::thread_with_file::StdioRedirect> {
    let stdio = fs.stdio.load(Ordering::Acquire);
    let filter = fs.stdio_filter.load(Ordering::Relaxed);
    if stdio.is_null() ||
       (!user && fs.stdio_user_only.load(Ordering::Relaxed)) ||
       (!filter.is_null() && filter != crate::os::sched::current()) {
        return None;
    }
    // SAFETY: the filesystem's redirect, set while it's attached
    Some(unsafe { &*stdio })
}

/// @s at printk level @level - None, the previous message's - if the
/// filesystem's loglevel lets it through. Indent left after the last newline
/// isn't printed.
pub fn print_bytes(fs: &Fs, level: Option<u8>, s: &[u8]) {
    let trimmed = s.len() - s.iter().rev().take_while(|&&b| b == b' ').count();
    let len = if trimmed == 0 || s[trimmed - 1] == b'\n' { trimmed } else { s.len() };
    if len == 0 {
        return;
    }

    let level = match level {
        Some(l) => {
            fs.prev_loglevel.store(l.into(), Ordering::Relaxed);
            u32::from(l)
        }
        None => fs.prev_loglevel.load(Ordering::Relaxed),
    };
    if level > max_loglevel(fs) {
        return;
    }

    #[cfg(kernel)]
    if let Some(stdio) = stdio_redirect(fs, false) {
        let _ = stdio.write(true, &s[..len]);
        return;
    }

    #[cfg(kernel)]
    let prefix: &[u8] = if level < 8 { &[0x01, b'0' + level as u8] } else { &[0x01] };
    #[cfg(not(kernel))]
    let prefix: &[u8] = &[];
    print_lines(prefix, s)
}

/// @s for the person running the fsck - their redirect, if they have one,
/// else the log at err.
pub fn print_user(fs: &Fs, s: &[u8]) {
    #[cfg(kernel)]
    if let Some(stdio) = stdio_redirect(fs, true) {
        let _ = stdio.write(true, s);
        return;
    }
    print_bytes(fs, Some(LogLevel::ERR.num()), s)
}

/// The next chunk of @lines for printk(): whole lines, as many as fit in
/// @limit bytes - the first, if it alone doesn't - and at the end, up to the
/// last line that isn't blank. Its length; None if only blank lines are left.
fn lines_under(lines: &[u8], limit: usize) -> Option<usize> {
    let (mut start, mut nonblank) = (0, None);
    loop {
        let end = lines[start..].iter().position(|&b| b == b'\n').map_or(lines.len(), |i| start + i);
        if lines[start..end].iter().any(|&b| b != b' ') {
            nonblank = Some(end);
        }
        if end == lines.len() {
            return nonblank;
        }
        if start != 0 && end > limit {
            return Some(start - 1);
        }
        start = end + 1;
    }
}

/// @lines to printk(), each chunk after @prefix.
pub fn print_lines(prefix: &[u8], lines: &[u8]) {
    // SAFETY: released below, if taken
    #[cfg(kernel)]
    let locked = unsafe { c::console_trylock() } != 0;

    let mut rest = lines;
    while let Some(n) = lines_under(rest, PRINTK_LIMIT) {
        crate::os::printk::printk_line(prefix, &rest[..n]);

        if n == rest.len() {
            break;
        }
        rest = &rest[n + 1..];
    }

    #[cfg(kernel)]
    if locked {
        unsafe { c::console_unlock() };
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

/// @name's prefix, in-kernel, and the continuation indent.
pub fn log_msg_start(out: &mut Printbuf, name: &CStr) {
    out.indent_add_nextline(2);
    #[cfg(kernel)]
    write!(out, "bcachefs ({}): ", name.to_str().unwrap_or("?"));
    #[cfg(not(kernel))]
    let _ = name;
}

impl<'a> LogMsg<'a> {
    /// At err.
    pub fn new(fs: &'a Fs) -> Self {
        Self::with_level(fs, LogLevel::ERR)
    }

    pub fn with_level(fs: &'a Fs, level: LogLevel) -> Self {
        let mut msg = Printbuf::new();
        log_msg_start(&mut msg, fs.name());
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
        print_bytes(self.fs, Some(self.level.num()), self.msg.as_bytes());
    }
}

impl Fs {
    /// Whether to drop a message from @func, by @rs: only with the
    /// ratelimit_errors option.
    pub fn ratelimit_suppress(&self, rs: &Ratelimit, func: &CStr) -> bool {
        self.opts.ratelimit_errors.get() != 0 && !rs.allow(func)
    }
}

/// As C's bch_log_ratelimited(): what the bch_err_ratelimited!() family
/// expands to, with ratelimit state per call site.
#[macro_export]
macro_rules! bch_log_ratelimited {
    ($fs:expr, $level:expr, $($arg:tt)*) => {{
        static RATELIMIT: $crate::util::ratelimit::Ratelimit = $crate::util::ratelimit::Ratelimit::new();
        let fs: &$crate::fs::Fs = $fs;
        if !fs.ratelimit_suppress(&RATELIMIT, $crate::here!()) {
            $crate::util::log::log(fs, $level, $crate::message!($($arg)*));
        }
    }};
}

/// A LogMsg at err, suppressed while this call site is ratelimited: C's
/// CLASS(bch_log_msg_ratelimited).
///
/// `log_msg_ratelimited!(fs, i, N)` gives the call site N ratelimit states and
/// uses the i'th: an IO error path's recovered errors and its failures each
/// get their own, so a stream of one can't ratelimit away the other.
#[macro_export]
macro_rules! log_msg_ratelimited {
    ($fs:expr) => {
        $crate::log_msg_ratelimited!($fs, 0, 1)
    };
    ($fs:expr, $idx:expr, $n:expr) => {{
        static RATELIMIT: [$crate::util::ratelimit::Ratelimit; $n] =
            [const { $crate::util::ratelimit::Ratelimit::new() }; $n];
        let fs: &$crate::fs::Fs = $fs;
        let mut msg = $crate::util::log::LogMsg::new(fs);
        msg.set_suppressed(fs.ratelimit_suppress(&RATELIMIT[$idx], $crate::here!()));
        msg
    }};
}

#[macro_export]
macro_rules! bch_err_ratelimited {
    ($fs:expr, $($arg:tt)*) => {
        $crate::bch_log_ratelimited!($fs, $crate::util::log::LogLevel::ERR, $($arg)*)
    };
}

#[macro_export]
macro_rules! bch_err_dev_ratelimited {
    ($ca:expr, $($arg:tt)*) => {{
        static RATELIMIT: $crate::util::ratelimit::Ratelimit = $crate::util::ratelimit::Ratelimit::new();
        let ca: &$crate::fs::Dev = $ca;
        if !ca.fs().ratelimit_suppress(&RATELIMIT, $crate::here!()) {
            $crate::util::log::log_dev(ca, $crate::util::log::LogLevel::ERR, $crate::message!($($arg)*));
        }
    }};
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

/// As C's WARN_ON_ONCE(): warn_on!(), the first time @cond holds at this
/// call site. Evaluates to @cond.
#[macro_export]
macro_rules! warn_on_once {
    ($cond:expr) => {{
        static WARNED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
        let cond: bool = $cond;
        if cond && !WARNED.swap(true, core::sync::atomic::Ordering::Relaxed) {
            $crate::warn_on!(true);
        }
        cond
    }};
}

/// As C's WARN_ONCE(): warn_on_once!(), with @msg logged before the warning -
/// of @fs, or of none: no_fs. Evaluates to @cond.
#[macro_export]
macro_rules! warn_once {
    (@once $cond:expr, $log:expr) => {{
        static WARNED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
        let cond: bool = $cond;
        if cond && !WARNED.swap(true, core::sync::atomic::Ordering::Relaxed) {
            $log;
            $crate::warn_on!(true);
        }
        cond
    }};
    ($cond:expr, no_fs, $($msg:tt)*) => {
        $crate::warn_once!(@once $cond,
            $crate::util::log::log_no_fs($crate::util::log::LogLevel::ERR, $crate::message!($($msg)*)))
    };
    ($cond:expr, $fs:expr, $($msg:tt)*) => {
        $crate::warn_once!(@once $cond, $crate::bch_err!($fs, $($msg)*))
    };
}

/// This call site, "file:line", as a C string that lives forever: what a
/// message says it came from, where C passes __func__.
#[macro_export]
macro_rules! here {
    () => {
        const {
            match ::core::ffi::CStr::from_bytes_with_nul(
                ::core::concat!(::core::file!(), ":", ::core::line!(), "\0").as_bytes()
            ) {
                Ok(s) => s,
                Err(_) => panic!("a NUL in file!()"),
            }
        }
    };
}

/// here!(), as a str: for a message that prints it.
#[macro_export]
macro_rules! here_str {
    () => {
        ::core::concat!(::core::file!(), ":", ::core::line!())
    };
}

/// As C's bch_err_fn(): report an error from @result, saying where - unless
/// it's a restart, which isn't one. Evaluates to @result, so it can
/// wrap the call: `bch_err_fn!(fs, f())?`.
#[macro_export]
macro_rules! bch_err_fn {
    ($fs:expr, $result:expr) => {{
        let result = $result;
        if let Err(e) = &result {
            let e: $crate::errcode::BchError = (*e).into();
            if $crate::util::log::should_print_err(&e) {
                $crate::bch_err!($fs, "{}: error {}", $crate::here_str!(), e.msg());
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
                $crate::bch_err!($fs, "{}: error {} {}", $crate::here_str!(),
                                 $crate::message!($($arg)*), e.msg());
            }
        }
        result
    }};
}

#[macro_export]
macro_rules! bch_err_dev {
    ($ca:expr, $($arg:tt)*) => {
        $crate::util::log::log_dev($ca, $crate::util::log::LogLevel::ERR, $crate::message!($($arg)*))
    };
}

#[macro_export]
macro_rules! bch_info_dev {
    ($ca:expr, $($arg:tt)*) => {
        $crate::util::log::log_dev($ca, $crate::util::log::LogLevel::INFO, $crate::message!($($arg)*))
    };
}

/// As C's bch_err_fn_dev(): bch_err_fn!(), after the device's name.
#[macro_export]
macro_rules! bch_err_fn_dev {
    ($ca:expr, $result:expr) => {{
        let result = $result;
        if let Err(e) = &result {
            let e: $crate::errcode::BchError = (*e).into();
            if $crate::util::log::should_print_err(&e) {
                $crate::bch_err_dev!($ca, "{}: error {}", $crate::here_str!(), e.msg());
            }
        }
        result
    }};
}

/// As C's bch_err_msg_dev(): bch_err_msg!(), after the device's name.
#[macro_export]
macro_rules! bch_err_msg_dev {
    ($ca:expr, $result:expr, $($arg:tt)*) => {{
        let result = $result;
        if let Err(e) = &result {
            let e: $crate::errcode::BchError = (*e).into();
            if $crate::util::log::should_print_err(&e) {
                $crate::bch_err_dev!($ca, "{}: error {} {}", $crate::here_str!(),
                                     $crate::message!($($arg)*), e.msg());
            }
        }
        result
    }};
}
