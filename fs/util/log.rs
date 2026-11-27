// SPDX-License-Identifier: GPL-2.0

//! Logging, as C's bch_log() family was: a message goes where the filesystem
//! sends it - past its loglevel, to a stdio redirect, at the "previous
//! loglevel" a continuation line inherits.
//!
//! In-kernel each message is prefixed with the filesystem's name; userspace
//! prints it bare.

#[cfg(kernel)]
use crate::c;
use crate::fs::{Dev, Fs};
use crate::opts::Opts;
use crate::errcode::BchError;
use crate::util::{ratelimit::Ratelimit, Printbuf, ToText};
use core::panic::Location;
use core::sync::atomic::Ordering;

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

/// @msg at printk level @level, after the filesystem's name if @prefix, and
/// a newline if @newline, in one printk: Log::info_start() and cont()'s, a line
/// a KERN_CONT finishes.
fn print(fs: &Fs, level: u8, prefix: bool, msg: &dyn ToText, newline: bool) {
    // KERN_CONT, anything not a digit, is at the previous level
    if loglevel(fs, level.is_ascii_digit().then(|| level - b'0')).is_none() {
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
    crate::os::printk::printk(buf.as_c_str())
}

/// The level a message at @level - None, the previous message's - prints at,
/// if the filesystem's loglevel lets it through. A level given is the one
/// what follows continues at.
fn loglevel(fs: &Fs, level: Option<u8>) -> Option<u32> {
    let level = match level {
        Some(l) => {
            fs.prev_loglevel.store(l.into(), Ordering::Relaxed);
            u32::from(l)
        }
        None => fs.prev_loglevel.load(Ordering::Relaxed),
    };
    let max = match fs.loglevel.load(Ordering::Relaxed) {
        0 if fs.opts().verbose() => 7,
        0 => 6,
        l => l,
    };
    (level <= max).then_some(level)
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
fn print_bytes(fs: &Fs, level: Option<u8>, s: &[u8]) {
    let trimmed = s.len() - s.iter().rev().take_while(|&&b| b == b' ').count();
    let len = if trimmed == 0 || s[trimmed - 1] == b'\n' { trimmed } else { s.len() };
    if len == 0 {
        return;
    }

    let Some(level) = loglevel(fs, level) else { return };

    #[cfg(kernel)]
    if let Some(stdio) = stdio_redirect(fs, false) {
        let _ = stdio.write(true, &s[..len]);
        return;
    }

    #[cfg(kernel)]
    let prefix: &[u8] = if level < 8 { &[0x01, b'0' + level as u8] } else { &[0x01] };
    #[cfg(not(kernel))]
    let prefix: &[u8] = { let _ = level; &[] };
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
fn print_lines(prefix: &[u8], lines: &[u8]) {
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
/// print_bytes(), unless suppressed - ratelimited, or by count_fsck_err(), when
/// it's a repeat. Log::msg() starts one; Log::at() is one written whole.
pub struct LogMsg<'a> {
    log:   Log<'a>,
    level: LogLevel,
    msg:   Printbuf,
}

impl<'a> LogMsg<'a> {
    /// For a message whose level depends on how things turned out.
    pub fn set_level(&mut self, level: LogLevel) {
        self.level = level;
    }

    pub fn with_level(mut self, level: LogLevel) -> Self { self.level = level; self }
    pub fn notice(mut self) -> Self  { self.level = LogLevel::NOTICE; self }
    pub fn info(mut self) -> Self    { self.level = LogLevel::INFO; self }
    pub fn verbose(mut self) -> Self { self.level = LogLevel::DEBUG; self }
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
        self.log.print(self.level, self.msg.as_bytes());
    }
}

impl Fs {
    /// Whether to drop a message from @func, by @rs: only with the
    /// ratelimit_errors option.
    pub fn ratelimit_suppress(&self, rs: &Ratelimit, at: &dyn core::fmt::Display) -> bool {
        self.opts.ratelimit_errors.get() != 0 && !rs.allow(at)
    }
}

/// @log, ratelimited by a state of this call site's own, as C's
/// bch_err_ratelimited() has: `ratelimit!(fs.log()).err(message!(...))`.
#[macro_export]
macro_rules! ratelimit {
    ($log:expr) => {{
        static RATELIMIT: $crate::util::ratelimit::Ratelimit = $crate::util::ratelimit::Ratelimit::new();
        $log.ratelimited(&RATELIMIT)
    }};
}

/// Where a message goes: the filesystem's log, or a device's - after its name
/// - maybe ratelimited. @msg is anything ToText; message!() makes one from a
/// format string.
///
/// The levels are methods, not a LogLevel argument, because a call naming
/// `LogLevel::ERR` needs LogLevel imported, though the signature already says
/// which type it is: Rust doesn't resolve a name against the expected type.
/// Rust devs, please fix this.
#[derive(Clone, Copy)]
pub struct Log<'a> {
    to:        To<'a>,
    ratelimit: Option<(&'a Ratelimit, &'static Location<'static>)>,
}

/// Whose log: a filesystem's - or a device's, after its name - with its
/// loglevel and stdio redirect; @opts', before there's a filesystem, which
/// may be a fsck's redirect; or none, printk() as it stands.
#[derive(Clone, Copy)]
enum To<'a> {
    Fs(&'a Fs),
    Dev(&'a Dev),
    Opts(&'a Opts),
    NoFs,
}

impl Fs {
    pub fn log(&self) -> Log<'_> {
        Log { to: To::Fs(self), ratelimit: None }
    }
}

impl Dev {
    pub fn log(&self) -> Log<'_> {
        Log { to: To::Dev(self), ratelimit: None }
    }
}

impl Opts {
    /// The log of a filesystem not yet opened with these options.
    pub fn log(&self) -> Log<'_> {
        Log { to: To::Opts(self), ratelimit: None }
    }
}

impl<'a> Log<'a> {
    /// No filesystem's: there isn't one, or it isn't known - printk().
    pub const NO_FS: Log<'static> = Log { to: To::NoFs, ratelimit: None };

    /// Suppressed while @rs is, which names this call site when it is: a
    /// state of the filesystem's, or one of the call site's own -
    /// ratelimit!().
    #[track_caller]
    pub fn ratelimited(self, rs: &'a Ratelimit) -> Self {
        Log { ratelimit: Some((rs, Location::caller())), ..self }
    }

    fn fs(&self) -> Option<&'a Fs> {
        match self.to {
            To::Fs(fs)  => Some(fs),
            To::Dev(ca) => Some(ca.fs()),
            _           => None,
        }
    }

    /// Whether the ratelimit, if there is one, drops this message: with the
    /// ratelimit_errors option, where there are options.
    fn suppressed(&self) -> bool {
        let Some((rs, at)) = self.ratelimit else { return false };
        let enabled = match self.to {
            To::Fs(fs)     => fs.opts.ratelimit_errors.get() != 0,
            To::Dev(ca)    => ca.fs().opts.ratelimit_errors.get() != 0,
            To::Opts(opts) => opts.ratelimit_errors != 0,
            To::NoFs       => true,
        };
        enabled && !rs.allow(at)
    }

    /// A message at err, built up and printed when dropped - suppressed now,
    /// if it's ratelimited and over the limit.
    pub fn msg(self) -> LogMsg<'a> {
        let mut msg = Printbuf::new();
        msg.indent_add_nextline(2);
        self.prefix(&mut msg);
        msg.set_suppressed(self.suppressed());
        LogMsg { log: self, level: LogLevel::ERR, msg }
    }

    /// Whose message it is: in-kernel, the filesystem's or device's name -
    /// a device's in userspace too, where the filesystem's is implicit.
    fn prefix(&self, out: &mut Printbuf) {
        match self.to {
            #[cfg(kernel)]
            To::Fs(fs)  => { write!(out, "bcachefs ({}): ", fs.name().to_str().unwrap_or("?")); }
            #[cfg(kernel)]
            To::Dev(ca) => { write!(out, "bcachefs ({ca}): "); }
            #[cfg(not(kernel))]
            To::Dev(ca) => { write!(out, "{ca}: "); }
            _ => {}
        }
    }

    /// @msg's bytes, at @level, where this log goes.
    fn print(&self, level: LogLevel, msg: &[u8]) {
        match self.to {
            To::Fs(fs)  => print_bytes(fs, Some(level.num()), msg),
            To::Dev(ca) => print_bytes(ca.fs(), Some(level.num()), msg),
            To::Opts(opts) => {
                #[cfg(kernel)]
                if opts.stdio != 0 {
                    // SAFETY: @opts' stdio is the fsck thread's redirect, live
                    // for as long as its opts are
                    let stdio = unsafe { &*(opts.stdio as usize as *const crate::os::thread_with_file::StdioRedirect) };
                    let _ = stdio.write(true, msg);
                    return;
                }
                #[cfg(not(kernel))]
                let _ = opts;
                print_lines(&level.prefix(), msg)
            }
            To::NoFs => print_lines(&level.prefix(), msg),
        }
    }

    pub fn at(self, level: LogLevel, msg: &dyn ToText) {
        let mut m = self.msg();
        m.level = level;
        msg.to_text(&mut m);
    }

    /// @msg at info, with no newline: a line for cont() to finish, in one
    /// printk each - KERN_CONT. Not ratelimited, and a filesystem's.
    pub fn info_start(self, msg: &dyn ToText) {
        match self.fs() {
            Some(fs) => print(fs, LogLevel::INFO.0, true, msg, false),
            None     => self.info(msg),
        }
    }

    /// The rest of the line info_start() began.
    pub fn cont(self, msg: &dyn ToText) {
        if let Some(fs) = self.fs() {
            print(fs, b'c', false, msg, true)
        }
    }

    pub fn err(self, msg: &dyn ToText)     { self.at(LogLevel::ERR, msg) }
    pub fn warn(self, msg: &dyn ToText)    { self.at(LogLevel::WARN, msg) }
    pub fn notice(self, msg: &dyn ToText)  { self.at(LogLevel::NOTICE, msg) }
    pub fn info(self, msg: &dyn ToText)    { self.at(LogLevel::INFO, msg) }
    /// As C's bch_verbose(): KERN_DEBUG.
    pub fn verbose(self, msg: &dyn ToText) { self.at(LogLevel::DEBUG, msg) }

    /// As C's bch_err_fn(): report an error from @result, saying where -
    /// unless it's a restart, which isn't one. @result, so it can wrap the
    /// call: `fs.log().err_fn(f())?`.
    #[track_caller]
    pub fn err_fn<T, E: Copy + Into<BchError>>(self, result: Result<T, E>) -> Result<T, E> {
        if let Err(e) = &result {
            let e: BchError = (*e).into();
            if !e.is_restart() {
                let at = Location::caller();
                self.err(crate::message!("{}:{}: error {}", at.file(), at.line(), e.msg()));
            }
        }
        result
    }

    /// err_fn(), saying what we were doing.
    #[track_caller]
    pub fn err_msg<T, E: Copy + Into<BchError>>(self, result: Result<T, E>, msg: &dyn ToText) -> Result<T, E> {
        if let Err(e) = &result {
            let e: BchError = (*e).into();
            if !e.is_restart() {
                let at = Location::caller();
                self.err(crate::message!("{}:{}: error {} {}", at.file(), at.line(), msg, e.msg()));
            }
        }
        result
    }
}

/// As C's WARN_ON(): the port's warning, at the call site. Evaluates to
/// @cond.
#[macro_export]
macro_rules! warn_on {
    ($cond:expr) => {{
        let cond: bool = $cond;
        if cond {
            $crate::os::printk::warn_at(const { $crate::util::ffi::c_str(concat!(file!(), "\0")) }, line!());
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
            $crate::util::log::Log::NO_FS.err($crate::message!($($msg)*)))
    };
    ($cond:expr, $fs:expr, $($msg:tt)*) => {
        $crate::warn_once!(@once $cond, $fs.log().err($crate::message!($($msg)*)))
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
