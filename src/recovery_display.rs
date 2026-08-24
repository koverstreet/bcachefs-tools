//! The block mount.bcachefs draws while a filesystem is coming up.
//!
//! Until the mount returns there is no sysfs and nothing in its own output to
//! say whether recovery is working or wedged; BCH_IOCTL_RECOVERY_STATUS is the
//! only thing that can say.
//!
//! Whether we draw decides whether we poll at all: the kernel stops logging
//! progress to dmesg as soon as anything reads that ioctl, on the grounds that
//! whoever read it is showing it.
//!
//! poll() on the status fd means "text to read", not "progress moved", so the
//! numbers are one interval stale.

use std::{
    ffi::CStr,
    fmt::Write as _,
    fs::{File, OpenOptions},
    io::{self, IsTerminal, Write},
    os::fd::BorrowedFd,
    path::Path,
    time::{Duration, Instant},
};

use rustix::fs::{flock, FlockOperation};

use bch_bindgen::c;

use crate::commands::fs_usage::replicas_spare_redundancy;
use crate::thread_with_file::StatusDisplay;
use crate::wrappers::accounting::data_type;
use crate::wrappers::ioctl::{
    ioctl_ptr, ioctl_rw, IoctlBuf, BCH_IOCTL_FS_USAGE, BCH_IOCTL_RECOVERY_STATUS,
};
use crate::wrappers::sysfs::DevInfo;

/// systemd repaints its own job line on the same row every 333ms
/// (JOBS_IN_PROGRESS_PERIOD_USEC), and nothing arbitrates. 50ms is what
/// systemd-fsck uses to win that race.
const INTERVAL: Duration = Duration::from_millis(50);
const BAR_WIDTH: usize = 20;

/// Every line is truncated to the width: a wrapped line occupies two rows, and
/// the redraw's cursor arithmetic is off by one from then on.
const FALLBACK_COLS: usize = 80;
const MIN_COLS: usize = 20;

/// In descending order of how many rows we get. The console gets one, because
/// systemd is drawing its own self-overwriting line there and cannot be told to
/// make room - there is no way for a unit to hand systemd a status to render.
/// See task #249.
enum Sink {
    Terminal { out: io::Stderr, rows: u16 },
    /// A display-message replaces the previous one in place, so the block goes
    /// here whole - nothing to erase, hence no counter.
    Plymouth,
    Console { file: File, cols: usize },
}

const PLYMOUTH_MAX: usize = crate::plymouth::MAX;

/// /run/systemd/show-status is PID 1 saying it is displaying status; it removes
/// the file when the admin asked for quiet. Honouring it is how `quiet` keeps
/// meaning something once we've stopped going through printk.
fn open_console() -> Option<File> {
    if !Path::new("/run/systemd/show-status").exists() {
        return None;
    }

    OpenOptions::new().write(true).open("/dev/console").ok()
}

pub struct RecoveryDisplay<'a> {
    fd:     BorrowedFd<'a>,
    sink:   Sink,
    source: String,

    /// The member devices as the scan found them - see
    /// device_scan::devices_from_superblocks(). Fixed for the life of a mount.
    devs: Vec<DevInfo>,

    devices_line: Option<String>,

    /// Set from the first status we manage to read, not from when we started
    /// polling: the fd exists from fsconfig(status_fd), which is before there
    /// is a filesystem behind it.
    started: Option<Instant>,

    /// Cleared on ENOTTY - a kernel without the ioctl, so stop asking.
    supported: bool,
}

impl<'a> RecoveryDisplay<'a> {
    /// None when there is nowhere to draw; the caller must then not poll
    /// either, for the reason at the top of this file.
    pub fn new(fd: BorrowedFd<'a>, source: String, devs: Vec<DevInfo>) -> Option<Self> {
        // Connect, don't send: an empty display-message is a real one, and it
        // blanks the splash for as long as it takes us to draw the first block.
        let sink = if io::stderr().is_terminal() {
            Sink::Terminal { out: io::stderr(), rows: 0 }
        } else if crate::plymouth::connect().is_ok() {
            Sink::Plymouth
        } else {
            Sink::Console { file: open_console()?, cols: 0 }
        };

        Some(RecoveryDisplay {
            fd,
            sink,
            source,
            devs,
            devices_line: None,
            started: None,
            supported: true,
        })
    }

    /// Redundancy is unknown, not zero, until accounting_read - the replicas
    /// entries live in accounting. Saying "0" during a degraded mount because
    /// we haven't looked yet is the worst thing this could do.
    ///
    /// Cached, not polled: accounting settles under mark_lock and doesn't move
    /// again.
    fn devices_line(&mut self, s: &c::bch_ioctl_recovery_status) -> String {
        if let Some(line) = &self.devices_line {
            return line.clone();
        }

        let online = self.devs.iter().filter(|d| d.online).count();
        let head = format!("mounting with {}/{} devices", online, self.devs.len());

        if !pass_complete(s, c::bch_recovery_pass::BCH_RECOVERY_PASS_accounting_read) {
            return format!("{head}, redundancy unknown (reading accounting)");
        }

        let line = match self.read_redundancy() {
            Some(r) => format!("{head}, current redundancy {r}"),
            None    => format!("{head}, redundancy unavailable"),
        };
        self.devices_line = Some(line.clone());
        line
    }

    /// How many more devices can go before some data is unreadable: the
    /// worst-off replicas entry decides, so the minimum over all of them.
    fn read_redundancy(&self) -> Option<i32> {
        let usage = fs_usage(self.fd).ok()?;

        usage.replicas()
            .filter(|r| r.r.data_type as u32 != u32::from(data_type::cached))
            .map(|r| {
                let devs = unsafe {
                    std::slice::from_raw_parts(r.r.devs.as_ptr(), r.r.nr_devs as usize)
                };
                replicas_spare_redundancy(r.r.nr_devs, r.r.nr_required, devs, &self.devs)
            })
            .min()
    }

    /// ENODEV is ordinary, not a failure: the ioctl only answers while a
    /// filesystem is coming up on this channel.
    fn read(&mut self) -> Option<c::bch_ioctl_recovery_status> {
        if !self.supported {
            return None;
        }

        let mut arg = c::bch_ioctl_recovery_status::default();

        match ioctl_rw::<BCH_IOCTL_RECOVERY_STATUS>(self.fd, &mut arg) {
            Ok(_) => {
                self.started.get_or_insert_with(Instant::now);
                Some(arg)
            }
            Err(e) => {
                self.supported = e.raw_os_error() != Some(libc::ENOTTY);
                None
            }
        }
    }

    fn fields(&mut self, s: &c::bch_ioctl_recovery_status) -> Fields<'_> {
        let running = s.pass != 0;

        // The denominator is what this run will have touched, which grows if a
        // pass reschedules an earlier one - so the count can go backwards, and
        // that is the truth rather than a glitch.
        let complete = mask_count(&s.passes_complete);
        let remaining = mask_count(&s.passes_remaining);

        let elapsed = self.started.map_or(Duration::ZERO, |t| t.elapsed());
        let devices = self.devices_line(s);

        Fields {
            source:     &self.source,
            pass:       running.then(|| pass_name(s.pass)),
            done:       complete + u32::from(running),
            total:      complete + remaining + u32::from(running),
            elapsed,
            seen:       s.seen,
            pass_total: s.total,
            units:      units_name(s.units),
            devices,
        }
    }

    fn emit(&mut self, lines: &[String]) -> io::Result<()> {
        match &mut self.sink {
            Sink::Terminal { out, rows } => {
                // `\x1b[{n}F` is n rows up, column zero; `\x1b[J` clears to end
                // of display. One write, because two would show the gap.
                let mut buf = String::new();

                if *rows > 0 {
                    let _ = write!(buf, "\x1b[{rows}F\x1b[J");
                }
                for l in lines {
                    buf.push_str(l);
                    buf.push('\n');
                }

                *rows = lines.len() as u16;
                out.write_all(buf.as_bytes())?;
                out.flush()
            }
            Sink::Plymouth => {
                // No erase: a display-message replaces the one before it, so
                // the empty block at the end clears the splash by itself.
                let mut msg = String::new();
                for l in lines {
                    if msg.len() + l.len() + 1 > PLYMOUTH_MAX {
                        break;
                    }
                    if !msg.is_empty() {
                        msg.push('\n');
                    }
                    msg.push_str(l);
                }

                // The splash may have exited; not the mount's problem.
                let _ = crate::plymouth::send(&msg);
                Ok(())
            }
            Sink::Console { file, cols } => {
                // Only one recovering filesystem draws. Re-taken rather than
                // remembered, so whoever is second picks the line up when the
                // first finishes; re-locking our own fd is a no-op.
                if flock(&*file, FlockOperation::NonBlockingLockExclusive).is_err() {
                    return Ok(());
                }

                // No newline in either direction: return to column zero, lay
                // the line down, return again. Trailing spaces cover whatever
                // of the previous line was longer, because without a newline
                // nothing else erases it.
                let line = lines.first().map(String::as_str).unwrap_or("");
                let pad  = cols.saturating_sub(line.chars().count());

                let mut buf = String::with_capacity(line.len() + pad + 2);
                buf.push('\r');
                buf.push_str(line);
                buf.extend(std::iter::repeat_n(' ', pad));
                buf.push('\r');

                *cols = line.chars().count();
                file.write_all(buf.as_bytes())?;
                file.flush()
            }
        }
    }
}

impl StatusDisplay for RecoveryDisplay<'_> {
    fn interval(&self) -> Duration {
        INTERVAL
    }

    fn erase(&mut self) -> io::Result<()> {
        self.emit(&[])
    }

    fn draw(&mut self) -> io::Result<()> {
        // The console isn't ours to measure - crossterm would be asking stderr,
        // which is not where this is going - so assume the conservative width.
        let console = matches!(self.sink, Sink::Console { .. });
        let cols = if console {
            FALLBACK_COLS
        } else {
            // Ok(0) is not a width. A terminal with no winsize set - a serial
            // console, hvc0 under a VM - answers the query successfully and
            // says zero, which truncate() turns into a block of " ..." with
            // every line's content thrown away. Only a plausible answer counts.
            crossterm::terminal::size()
                .ok()
                .map(|(c, _)| c as usize)
                .filter(|&c| c >= MIN_COLS)
                .unwrap_or(FALLBACK_COLS)
        };

        match self.read() {
            Some(s) => {
                let f = self.fields(&s);
                let lines = if console {
                    vec![render_line(&f, cols)]
                } else {
                    render_block(&f, cols)
                };
                self.emit(&lines)
            }
            None => self.emit(&[]),
        }
    }
}

/// Everything either layout could want, worked out once. The block and the
/// line differ in how much room they have, not in what they have to say.
struct Fields<'a> {
    source:  &'a str,
    pass:    Option<&'static str>,
    done:    u32,
    total:   u32,
    elapsed: Duration,

    /// Progress within `pass`. pass_total is zero when the pass can't estimate
    /// its own size, which is a bare count instead of a bar.
    seen:       u64,
    pass_total: u64,
    units:      &'static str,

    devices: String,
}

impl Fields<'_> {
    fn pct(&self) -> Option<u64> {
        (self.pass_total != 0).then(|| self.seen * 100 / self.pass_total)
    }
}

/// Most-useful-first, because the truncation at the end drops the tail. A
/// percentage is deliberately not the headline: 57% of one pass out of fifty
/// implies something about how much longer, and it's wrong.
fn render_block(f: &Fields, cols: usize) -> Vec<String> {
    let mut lines = vec![
        format!("Recovering {}: {} of {} passes, {}",
                f.source, f.done, f.total, fmt_elapsed(f.elapsed)),
        format!("  {}", f.devices),
    ];

    if let Some(pass) = f.pass {
        let mut l = format!("  {pass}");

        let _ = match f.pct() {
            Some(pct) => write!(l, "  {:>3}% [{}] {}/{} {}",
                                pct, bar(pct), f.seen, f.pass_total, f.units),
            None      => write!(l, "  {} {}", f.seen, f.units),
        };
        lines.push(l);
    }

    for l in &mut lines {
        truncate(l, cols);
    }
    lines
}

/// The same, folded onto one row.
fn render_line(f: &Fields, cols: usize) -> String {
    let mut l = format!("Recovering {}: ", f.source);

    if let Some(pass) = f.pass {
        let _ = write!(l, "{pass} ");
        if let Some(pct) = f.pct() {
            let _ = write!(l, "{pct}% ");
        }
    }

    let _ = write!(l, "({}/{} passes) {} - {}",
                   f.done, f.total, fmt_elapsed(f.elapsed), f.devices);

    truncate(&mut l, cols);
    l
}

fn pass_complete(s: &c::bch_ioctl_recovery_status, pass: c::bch_recovery_pass) -> bool {
    let n = pass as u32;

    s.passes_complete.v[(n / 64) as usize] & (1u64 << (n % 64)) != 0
}

/// BCH_IOCTL_FS_USAGE's reply: a fixed header followed by variable-size
/// replicas entries, each as long as its own device list.
struct FsUsage {
    buf:   IoctlBuf<c::bch_ioctl_fs_usage>,
    bytes: usize,
}

/// Rust's spelling of replicas_usage_bytes() - the C inline can't cross
/// bindgen, and the length has to come from the entry's own nr_devs because
/// that is what makes these variable-size.
fn replicas_usage_bytes(u: &c::bch_replicas_usage) -> usize {
    std::mem::offset_of!(c::bch_replicas_usage, r)
        + std::mem::size_of::<c::bch_replicas_entry_v1>()
        + u.r.nr_devs as usize
}

impl FsUsage {
    fn replicas(&self) -> impl Iterator<Item = &c::bch_replicas_usage> + '_ {
        let trailing = self.buf.trailing_bytes(self.bytes);
        let mut off = 0usize;

        std::iter::from_fn(move || {
            // A truncated tail is the kernel and us disagreeing about the
            // layout; stop rather than read past what it wrote.
            if off + std::mem::size_of::<c::bch_replicas_usage>() > trailing.len() {
                return None;
            }

            let u = unsafe { &*(trailing.as_ptr().add(off) as *const c::bch_replicas_usage) };
            let len = replicas_usage_bytes(u);

            if len == 0 || off + len > trailing.len() {
                return None;
            }
            off += len;
            Some(u)
        })
    }
}

/// One BCH_IOCTL_FS_USAGE call, growing the buffer until the reply fits.
///
/// FS_USAGE and not QUERY_ACCOUNTING because QUERY_ACCOUNTING answers keys you
/// name, and the replicas entries are exactly what we don't know in advance:
/// which combinations of devices exist is the question.
fn fs_usage(fd: BorrowedFd<'_>) -> io::Result<FsUsage> {
    let mut bytes = 4096usize;

    loop {
        let mut buf = IoctlBuf::<c::bch_ioctl_fs_usage>::new::<u8>(bytes);
        buf.hdr_mut().replica_entries_bytes = bytes as u32;

        match unsafe { ioctl_ptr::<BCH_IOCTL_FS_USAGE>(fd, buf.as_mut_ptr()) } {
            Ok(_) => {
                let n = buf.hdr().replica_entries_bytes as usize;
                return Ok(FsUsage { buf, bytes: n });
            }
            Err(e) if e.raw_os_error() == Some(libc::ERANGE) && bytes < 1 << 20 => {
                bytes *= 2;
            }
            Err(e) => return Err(e),
        }
    }
}

fn mask_count(m: &c::bch_recovery_pass_mask) -> u32 {
    m.v.iter().map(|w| w.count_ones()).sum()
}

/// bindgen types the unsized `bch2_recovery_passes[]` as `[*const c_char; 0]`,
/// so `.get()` returns None for every index, not just out-of-range ones - a
/// lookup that reads like a miss and cannot succeed. Index off the address.
fn pass_name(pass: u32) -> &'static str {
    if pass >= c::bch_recovery_pass::BCH_RECOVERY_PASS_NR as u32 {
        return "(unknown)";
    }

    unsafe {
        let p = *c::bch2_recovery_passes.as_ptr().add(pass as usize);
        if p.is_null() {
            return "(unknown)";
        }
        CStr::from_ptr(p).to_str().unwrap_or("(unknown)")
    }
}

fn units_name(units: u32) -> &'static str {
    if units == c::bch_progress_units::BCH_PROGRESS_UNITS_keys as u32 {
        "keys"
    } else {
        "nodes"
    }
}

fn bar(pct: u64) -> String {
    let filled = (pct as usize * BAR_WIDTH / 100).min(BAR_WIDTH);

    let mut s = "=".repeat(filled.saturating_sub(1));
    if filled > 0 {
        s.push('>');
    }
    while s.len() < BAR_WIDTH {
        s.push(' ');
    }
    s
}

fn fmt_elapsed(d: Duration) -> String {
    let s = d.as_secs();

    if s >= 3600 {
        format!("{}h{:02}m{:02}s", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}m{:02}s", s / 60, s % 60)
    }
}

/// Truncate to `cols` display columns, ellipsizing so it reads as cut short
/// rather than as a complete list. Pass names are ASCII, so bytes are columns.
fn truncate(s: &mut String, cols: usize) {
    if s.len() > cols {
        s.truncate(cols.saturating_sub(4));
        s.push_str(" ...");
    }
}
