// SPDX-License-Identifier: GPL-2.0

//! Logging, as C's bch_log() family (bcachefs.h): a message is formatted
//! here, then handed to __bch2_print() whole, so it goes where C's messages go -
//! the filesystem's loglevel, a stdio redirect, the "previous loglevel" a
//! continuation line inherits. A message printed as it stands - print_str(),
//! a LogMsg - is printed here: bch2_print_str(), which C calls too.
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
use crate::util::ffi::{c_char, Opaque};
use crate::util::{Printbuf, ToText};
use core::cell::UnsafeCell;
use core::ffi::{c_int, c_uint, CStr};
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, Ordering};
use cstruct_macros::{CStruct, c_enum, c_export, c_extern, TypeInfo};

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
    fn num(self) -> u8 {
        self.0 - b'0'
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
        let stdio = unsafe {
            crate::util::thread_with_file::StdioRedirect::from_c(opts.stdio as *mut c::stdio_redirect)
        };
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
    let s = buf.as_c_str();
    // SAFETY: a format, and the string it takes
    #[cfg(kernel)]
    unsafe { c::_printk(c"%s".as_ptr().cast(), s.as_ptr()) };
    // SAFETY: as the kernel's
    #[cfg(not(kernel))]
    unsafe { c::printk(c"%s".as_ptr().cast(), s.as_ptr()) };
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
    let mut buf = Printbuf::new();

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

    // KERN_SOH, level, then the message as data: a '%' in it is text.
    let fmt = [0x01, level, b'%', b's', 0];
    unsafe {
        __bch2_print(fs.as_ptr(), fmt.as_ptr() as *const c_char, buf.as_raw().buf);
    }
}

/// @msg at @level after device @ca's name, as C's bch_dev_log() - in
/// userspace too, unlike the filesystem's.
pub fn log_dev(ca: &c::bch_dev, level: LogLevel, msg: &dyn ToText) {
    let msg = crate::util::printbuf::Msg(|out: &mut Printbuf| {
        #[cfg(kernel)]
        write!(out, "bcachefs ({ca}): ");
        #[cfg(not(kernel))]
        write!(out, "{ca}: ");
        msg.to_text(out);
    });
    print(ca.fs(), level.0, false, &msg, true)
}

/// Print @msg at @level as it stands, one line at a time with no prefix -
/// C's bch2_print_str().
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
fn stdio_redirect(fs: &Fs, user: bool) -> Option<&crate::util::thread_with_file::StdioRedirect> {
    if fs.stdio.is_null() ||
       (!user && fs.stdio_user_only) ||
       (!fs.stdio_filter.is_null() && fs.stdio_filter != unsafe { c::get_current() }) {
        return None;
    }
    // SAFETY: the filesystem's redirect, set while it's attached
    Some(unsafe { crate::util::thread_with_file::StdioRedirect::from_c(fs.stdio) })
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

    // SAFETY: C's, written unlocked as C writes it
    let prev = unsafe { &raw mut (*fs.as_ptr()).prev_loglevel };
    let level = match level {
        Some(l) => {
            unsafe { *prev = l.into() };
            u32::from(l)
        }
        None => unsafe { *prev },
    };
    let max = match fs.loglevel {
        0 if fs.opts().verbose() => 7,
        0 => 6,
        l => l,
    };
    if level > max {
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
        let fmt = c"%.*s%.*s\n".as_ptr().cast();
        let (p, l) = (prefix.len() as c_int, n as c_int);
        // SAFETY: a format, and the lengths and bytes it takes
        #[cfg(kernel)]
        unsafe { c::_printk(fmt, p, prefix.as_ptr(), l, rest.as_ptr()) };
        #[cfg(not(kernel))]
        unsafe { c::printk(fmt, p, prefix.as_ptr(), l, rest.as_ptr()) };

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

/// C's string, or a printbuf that's never had any: nothing.
unsafe fn c_bytes<'a>(s: *const c_char) -> Option<&'a [u8]> {
    // SAFETY: the caller's
    unsafe { crate::util::ffi::opt_cstr(s) }.map(CStr::to_bytes)
}

/// KERN_SOH and a level, as a C KERN_* prefix has them: None for any other
/// prefix, a continuation.
fn prefix_level(prefix: &[u8]) -> Option<u8> {
    match prefix {
        [0x01, d @ b'0'..=b'9', ..] => Some(d - b'0'),
        _ => None,
    }
}

/// # Safety
/// @str is NULL or a string.
#[c_export]
pub unsafe fn bch2_print_str_loglevel(c: &Opaque<c::bch_fs>, loglevel: c_int, str: *const c_char) {
    if let Some(s) = unsafe { c_bytes(str) } {
        print_bytes(Fs::from_c(c), u8::try_from(loglevel).ok(), s)
    }
}

/// # Safety
/// @prefix a string; @str NULL or one.
#[c_export]
pub unsafe fn bch2_print_str(c: &Opaque<c::bch_fs>, prefix: *const c_char, str: *const c_char) {
    if let Some(s) = unsafe { c_bytes(str) } {
        let level = prefix_level(unsafe { c_bytes(prefix) }.unwrap_or_default());
        print_bytes(Fs::from_c(c), level, s)
    }
}

/// # Safety
/// @str is NULL or a string.
#[c_export]
pub unsafe fn bch2_print_str_user(c: &Opaque<c::bch_fs>, str: *const c_char) {
    if let Some(s) = unsafe { c_bytes(str) } {
        print_user(Fs::from_c(c), s)
    }
}

/// # Safety
/// @prefix a string; @lines NULL or one.
#[c_export]
pub unsafe fn bch2_print_string_as_lines(prefix: *const c_char, lines: *const c_char) {
    let prefix = unsafe { c_bytes(prefix) }.unwrap_or_default();
    match unsafe { c_bytes(lines) } {
        Some(lines) => print_lines(prefix, lines),
        None        => print_lines(prefix, b" (null)"),
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
    unsafe { __bch2_log_msg_start(name.as_ptr().cast(), out.as_raw()) }
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

/// A LogMsg at err, suppressed while this call site is ratelimited: C's
/// CLASS(bch_log_msg_ratelimited).
#[macro_export]
macro_rules! log_msg_ratelimited {
    ($fs:expr) => {{
        static RATELIMIT: $crate::util::log::Ratelimit = $crate::util::log::Ratelimit::new();
        let fs: &$crate::fs::Fs = $fs;
        let mut msg = $crate::util::log::LogMsg::new(fs);
        msg.set_suppressed(RATELIMIT.suppress(fs, $crate::c_function_name!()));
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

/// As C's WARN_ONCE(): warn_on_once!(), with @msg logged before the warning.
/// Evaluates to @cond.
#[macro_export]
macro_rules! warn_once {
    ($cond:expr, $fs:expr, $($msg:tt)*) => {{
        static WARNED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
        let cond: bool = $cond;
        if cond && !WARNED.swap(true, core::sync::atomic::Ordering::Relaxed) {
            $crate::bch_err!($fs, $($msg)*);
            $crate::warn_on!(true);
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

#[macro_export]
macro_rules! bch_err_dev {
    ($ca:expr, $($arg:tt)*) => {
        $crate::util::log::log_dev($ca, $crate::util::log::LogLevel::ERR, $crate::message!($($arg)*))
    };
}

/// As C's bch_err_msg_dev(): bch_err_msg!(), after the device's name.
#[macro_export]
macro_rules! bch_err_msg_dev {
    ($ca:expr, $result:expr, $($arg:tt)*) => {{
        let result = $result;
        if let Err(e) = &result {
            let e: $crate::errcode::BchError = (*e).into();
            if $crate::util::log::should_print_err(&e) {
                $crate::bch_err_dev!($ca, "{}(): error {} {}", $crate::function_name!(),
                                     $crate::message!($($arg)*), e.msg());
            }
        }
        result
    }};
}
