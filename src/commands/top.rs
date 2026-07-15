use std::collections::HashMap;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::mem;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use bch_bindgen::c::bch_ioctl_query_counters;
use bch_bindgen::sb::COUNTERS;
use clap::Parser;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{self, ClearType},
};
use owo_colors::OwoColorize;
use serde::Deserialize;

use crate::util::{fmt_bytes_human, fmt_num_human, run_tui};
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::ioctl::bch_ioc_w;
use crate::wrappers::sysfs::{dev_name_from_sysfs, sysfs_path_from_fd};

// ioctl constants

const BCH_IOCTL_QUERY_COUNTERS_NR: u32 = 21;
const BCH_IOCTL_QUERY_COUNTERS_MOUNT: u16 = 1 << 0;

// ioctl query

fn read_counters(fd: i32, flags: u16, nr_stable: u16) -> Result<Vec<u64>> {
    let hdr_size = mem::size_of::<bch_ioctl_query_counters>();
    let buf_size = hdr_size + (nr_stable as usize) * mem::size_of::<u64>();
    let mut buf = vec![0u8; buf_size];

    unsafe {
        let hdr = &mut *(buf.as_mut_ptr() as *mut bch_ioctl_query_counters);
        hdr.nr = nr_stable;
        hdr.flags = flags;
    }

    let request = bch_ioc_w::<bch_ioctl_query_counters>(BCH_IOCTL_QUERY_COUNTERS_NR);
    let ret = unsafe { libc::ioctl(fd, request, buf.as_mut_ptr()) };
    if ret < 0 {
        return Err(std::io::Error::last_os_error().into());
    }

    let actual_nr = unsafe { (*(buf.as_ptr() as *const bch_ioctl_query_counters)).nr } as usize;
    let data = unsafe { buf.as_ptr().add(hdr_size) as *const u64 };
    Ok((0..actual_nr).map(|i| unsafe { std::ptr::read_unaligned(data.add(i)) }).collect())
}

// Per-device IO from sysfs (io_done is JSON: {"read": {...}, "write": {...}}, values in bytes)

#[derive(Deserialize)]
struct IoDone {
    read:  HashMap<String, u64>,
    write: HashMap<String, u64>,
}

struct DevIoEntry {
    label:      String,     // "dev/data_type"
    read_bytes: u64,
    write_bytes: u64,
}

fn read_device_io(sysfs_path: &Path) -> Vec<DevIoEntry> {
    let mut entries = Vec::new();
    let Ok(dir) = fs::read_dir(sysfs_path) else { return entries };

    for entry in dir.flatten() {
        let dirname = entry.file_name().to_string_lossy().into_owned();
        if !dirname.starts_with("dev-") { continue }

        let dev_path = entry.path();
        let dev_name = dev_name_from_sysfs(&dev_path);

        let io_done_path = dev_path.join("io_done");
        let Ok(content) = fs::read_to_string(&io_done_path) else { continue };
        let Ok(io_done) = serde_json::from_str::<IoDone>(&content) else { continue };

        for (dtype, &r) in &io_done.read {
            let w = io_done.write.get(dtype).copied().unwrap_or(0);
            if r != 0 || w != 0 {
                entries.push(DevIoEntry {
                    label: format!("{}/{}", dev_name, dtype),
                    read_bytes: r,
                    write_bytes: w,
                });
            }
        }
    }
    entries.sort_by(|a, b| a.label.cmp(&b.label));
    entries
}

// Human-readable formatting

fn fmt_bytes(bytes: u64, human_readable: bool) -> String {
    if human_readable { fmt_bytes_human(bytes) } else { format!("{}", bytes) }
}

fn fmt_counter(val: u64, sectors: bool, human_readable: bool) -> String {
    if sectors {
        fmt_bytes(val << 9, human_readable)
    } else if human_readable && val >= 10_000 {
        fmt_num_human(val)
    } else {
        format!("{}", val)
    }
}

// CLI

#[derive(Parser, Debug)]
#[command(about = "Display runtime performance info", disable_help_flag = true)]
pub struct Cli {
    /// Print help
    #[arg(long = "help", action = clap::ArgAction::Help)]
    _help: (),

    /// Human-readable units
    #[arg(short, long)]
    human_readable: bool,

    /// One-shot output (no interactive TUI; equivalent to -n 1)
    #[arg(long)]
    once: bool,

    /// Number of samples to print, then exit (0 = unlimited / interactive TUI)
    #[arg(short = 'n', long, default_value = "0")]
    count: u32,

    /// Delay between samples, in seconds
    #[arg(short = 'd', long, default_value = "1")]
    delay: u32,

    /// Filesystem path, device, or UUID (default: current directory)
    filesystem: Option<String>,
}

// TUI state

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Base,
    Devices,
}

impl Page {
    const ALL: &'static [Page] = &[Page::Base, Page::Devices];
    fn label(self) -> &'static str {
        match self {
            Page::Base    => "counters",
            Page::Devices => "devices",
        }
    }
    fn next(self) -> Page {
        let i = Self::ALL.iter().position(|&p| p == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }
    fn prev(self) -> Page {
        let i = Self::ALL.iter().position(|&p| p == self).unwrap_or(0);
        Self::ALL[(i + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

struct TopState {
    ioctl_fd:       i32,
    nr_stable:      u16,
    mount_vals:     Vec<u64>,
    start_vals:     Vec<u64>,
    prev_vals:      Vec<u64>,
    prev_dev_io:    HashMap<String, (u64, u64)>,    // label -> (read, write)
    human_readable: bool,
    sysfs_path:     PathBuf,
    interval_secs:  u32,
    page:           Page,
    cursor:         usize,
    scroll_offset:  usize,
}

impl TopState {
    fn new(handle: &BcachefsHandle, human_readable: bool) -> Result<Self> {
        let ioctl_fd = handle.ioctl_fd_raw();
        let nr_stable = COUNTERS.iter().map(|c| c.stable_id).max().unwrap_or(0) + 1;

        let mount_vals = read_counters(ioctl_fd, BCH_IOCTL_QUERY_COUNTERS_MOUNT, nr_stable)?;
        let start_vals = read_counters(ioctl_fd, 0, nr_stable)?;
        let prev_vals  = read_counters(ioctl_fd, 0, nr_stable)?;

        let sysfs_path = sysfs_path_from_fd(handle.sysfs_fd())?;

        Ok(TopState {
            ioctl_fd, nr_stable,
            mount_vals, start_vals, prev_vals,
            prev_dev_io: HashMap::new(),
            human_readable,
            sysfs_path, interval_secs: 1,
            page: Page::Base,
            cursor: 0,
            scroll_offset: 0,
        })
    }

    fn get_val(vals: &[u64], stable_id: u16) -> u64 {
        let idx = stable_id as usize;
        if idx < vals.len() { vals[idx] } else { 0 }
    }
}

/* Build the current frame as Vec<String>, return the line index of the
 * cursor row so the caller can adjust scroll_offset to keep it visible.
 * Total visible rows on this page is also returned (for cursor clamping). */
fn build_frame(state: &TopState, curr: &[u64], dev_io: &[DevIoEntry])
    -> (Vec<String>, Option<usize>, usize)
{
    let mut lines = Vec::new();
    let mut cursor_line = None;
    let mut row = 0usize;

    lines.push("All counters have a corresponding tracepoint; for more info on any given event, try e.g.".into());
    lines.push("  trace-cmd stream -e bcachefs:data_update_pred".into());
    lines.push(String::new());
    lines.push("  q:quit  h:human-readable  Tab:page  \u{2191}\u{2193}:scroll  PgUp/PgDn  1-9:interval".into());

    /* Page tab bar */
    let mut tabs = String::from("  ");
    for (i, &p) in Page::ALL.iter().enumerate() {
        if i > 0 { tabs.push_str("  "); }
        let label = format!("[{}]", p.label());
        if p == state.page {
            tabs.push_str(&format!("{}", label.reversed()));
        } else {
            tabs.push_str(&label);
        }
    }
    lines.push(tabs);
    lines.push(String::new());

    let h = state.human_readable;
    let interval = state.interval_secs as u64;

    match state.page {
        Page::Base => {
            lines.push(format!("{:<40} {:>14} {:>14} {:>14}",
                "", format!("{}/s", state.interval_secs), "total", "mount"));

            for c in COUNTERS {
                let cv = TopState::get_val(curr, c.stable_id);
                let pv = TopState::get_val(&state.prev_vals, c.stable_id);
                let sv = TopState::get_val(&state.start_vals, c.stable_id);
                let mv = TopState::get_val(&state.mount_vals, c.stable_id);

                let v_mount = cv.wrapping_sub(mv);
                if v_mount == 0 { continue }

                let v_rate  = cv.wrapping_sub(pv);
                let v_total = cv.wrapping_sub(sv);

                let row_str = format!("{:<40} {:>12}/s {:>14} {:>14}",
                    c.name,
                    fmt_counter(v_rate / interval, c.is_sectors, h),
                    fmt_counter(v_total, c.is_sectors, h),
                    fmt_counter(v_mount, c.is_sectors, h));

                if row == state.cursor {
                    cursor_line = Some(lines.len());
                    lines.push(format!("{}{}", "\u{25ba} ".bold(), row_str.bold()));
                } else {
                    lines.push(format!("  {}", row_str));
                }
                row += 1;
            }
        }
        Page::Devices => {
            lines.push(format!("{:<40} {:>14} {:>14} {:>14} {:>14}",
                "", "read/s", "read", "write/s", "write"));

            for dev in dev_io {
                let (prev_r, prev_w) = state.prev_dev_io
                    .get(&dev.label)
                    .copied()
                    .unwrap_or((dev.read_bytes, dev.write_bytes));
                let rate_r = dev.read_bytes.wrapping_sub(prev_r) / interval;
                let rate_w = dev.write_bytes.wrapping_sub(prev_w) / interval;

                let row_str = format!("{:<40} {:>14} {:>14} {:>14} {:>14}",
                    &dev.label,
                    fmt_bytes(rate_r, h), fmt_bytes(dev.read_bytes, h),
                    fmt_bytes(rate_w, h), fmt_bytes(dev.write_bytes, h));

                if row == state.cursor {
                    cursor_line = Some(lines.len());
                    lines.push(format!("{}{}", "\u{25ba} ".bold(), row_str.bold()));
                } else {
                    lines.push(format!("  {}", row_str));
                }
                row += 1;
            }
        }
    }

    (lines, cursor_line, row)
}

fn render(state: &mut TopState, curr: &[u64], dev_io: &[DevIoEntry], stdout: &mut io::Stdout)
    -> io::Result<usize>
{
    let (_, term_h) = terminal::size().unwrap_or((120, 40));
    let visible = (term_h as usize).saturating_sub(1).max(1);

    let (lines, cursor_line, total_rows) = build_frame(state, curr, dev_io);

    if let Some(cl) = cursor_line {
        if cl < state.scroll_offset {
            state.scroll_offset = cl;
        } else if cl >= state.scroll_offset + visible {
            state.scroll_offset = cl - visible + 1;
        }
    } else if state.scroll_offset >= lines.len() {
        state.scroll_offset = lines.len().saturating_sub(1);
    }

    execute!(stdout, cursor::MoveTo(0, 0), terminal::Clear(ClearType::All))?;
    for line in lines.iter().skip(state.scroll_offset).take(visible) {
        write!(stdout, "{}\r\n", line)?;
    }
    stdout.flush()?;
    Ok(total_rows)
}

/* Print one frame: counters page (rate / total / mount) followed by devices
 * page (read/s / read / write/s / write). prev_* gives the baseline for
 * rates; on the first frame the caller passes curr as prev so rates are 0. */
fn print_frame(
    curr: &[u64], prev: &[u64], mount: &[u64],
    dev_io: &[DevIoEntry], prev_dev_io: &HashMap<String, (u64, u64)>,
    delay: u32, h: bool,
) {
    let d = delay as u64;

    println!("counters:");
    println!("  {:<40} {:>12}   {:>14} {:>14}",
        "", format!("{}/s", delay), "total", "mount");
    for c in COUNTERS {
        let cv = TopState::get_val(curr, c.stable_id);
        let pv = TopState::get_val(prev, c.stable_id);
        let mv = TopState::get_val(mount, c.stable_id);
        let v_mount = cv.wrapping_sub(mv);
        if v_mount == 0 { continue }

        let v_rate = cv.wrapping_sub(pv);
        println!("  {:<40} {:>12}/s {:>14} {:>14}",
            c.name,
            fmt_counter(v_rate / d, c.is_sectors, h),
            fmt_counter(cv,         c.is_sectors, h),
            fmt_counter(v_mount,    c.is_sectors, h));
    }

    if !dev_io.is_empty() {
        println!();
        println!("devices:");
        println!("  {:<40} {:>14} {:>14} {:>14} {:>14}",
            "", "read/s", "read", "write/s", "write");
        for dev in dev_io {
            let (pr, pw) = prev_dev_io
                .get(&dev.label)
                .copied()
                .unwrap_or((dev.read_bytes, dev.write_bytes));
            let rate_r = dev.read_bytes.wrapping_sub(pr) / d;
            let rate_w = dev.write_bytes.wrapping_sub(pw) / d;
            println!("  {:<40} {:>14} {:>14} {:>14} {:>14}",
                &dev.label,
                fmt_bytes(rate_r, h), fmt_bytes(dev.read_bytes,  h),
                fmt_bytes(rate_w, h), fmt_bytes(dev.write_bytes, h));
        }
    }
}

/* Non-interactive: take an initial sample, then for each of `count` frames
 * sleep `delay` seconds, take a fresh sample, and print rates against the
 * previous sample. Total samples taken = count + 1; total frames printed = count. */
fn run_non_interactive(
    handle: &BcachefsHandle, human_readable: bool,
    count: u32, delay: u32,
) -> Result<()> {
    let ioctl_fd   = handle.ioctl_fd_raw();
    let nr_stable  = COUNTERS.iter().map(|c| c.stable_id).max().unwrap_or(0) + 1;
    let mount_vals = read_counters(ioctl_fd, BCH_IOCTL_QUERY_COUNTERS_MOUNT, nr_stable)?;
    let sysfs_path = sysfs_path_from_fd(handle.sysfs_fd())?;

    let mut prev_vals   = read_counters(ioctl_fd, 0, nr_stable)?;
    let mut prev_dev_io: HashMap<String, (u64, u64)> = read_device_io(&sysfs_path)
        .into_iter()
        .map(|d| (d.label, (d.read_bytes, d.write_bytes)))
        .collect();

    for i in 0..count {
        std::thread::sleep(Duration::from_secs(delay as u64));
        let curr   = read_counters(ioctl_fd, 0, nr_stable)?;
        let dev_io = read_device_io(&sysfs_path);

        if i > 0 { println!(); }
        print_frame(&curr, &prev_vals, &mount_vals, &dev_io, &prev_dev_io, delay, human_readable);

        prev_vals   = curr;
        prev_dev_io = dev_io.into_iter()
            .map(|d| (d.label, (d.read_bytes, d.write_bytes)))
            .collect();
    }
    Ok(())
}

fn run_interactive(handle: BcachefsHandle, human_readable: bool, delay: u32) -> Result<()> {
    let mut state = TopState::new(&handle, human_readable)?;
    state.interval_secs = delay.max(1);

    run_tui(|stdout| loop {
        let curr = read_counters(state.ioctl_fd, 0, state.nr_stable)?;
        let dev_io = read_device_io(&state.sysfs_path);
        let total_rows = render(&mut state, &curr, &dev_io, stdout)?;
        state.prev_vals = curr;
        state.prev_dev_io = dev_io.into_iter()
            .map(|d| (d.label, (d.read_bytes, d.write_bytes)))
            .collect();

        /* Clamp cursor to current page's row count (e.g. counters can drop
         * out from under us when v_mount goes back to zero between ticks). */
        if total_rows > 0 && state.cursor >= total_rows {
            state.cursor = total_rows - 1;
        }

        if event::poll(Duration::from_secs(state.interval_secs as u64))? {
            if let Event::Key(key) = event::read()? {
                let (_, term_h) = terminal::size().unwrap_or((120, 40));
                let page_step = (term_h as usize).saturating_sub(1).max(1);

                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                    KeyCode::Tab => {
                        state.page = if key.modifiers.contains(KeyModifiers::SHIFT) {
                            state.page.prev()
                        } else {
                            state.page.next()
                        };
                        state.cursor = 0;
                        state.scroll_offset = 0;
                    }
                    KeyCode::BackTab => {
                        state.page = state.page.prev();
                        state.cursor = 0;
                        state.scroll_offset = 0;
                    }
                    KeyCode::Up   => state.cursor = state.cursor.saturating_sub(1),
                    KeyCode::Down => if total_rows > 0 {
                        state.cursor = (state.cursor + 1).min(total_rows - 1);
                    },
                    KeyCode::PageUp   => state.cursor = state.cursor.saturating_sub(page_step),
                    KeyCode::PageDown => if total_rows > 0 {
                        state.cursor = (state.cursor + page_step).min(total_rows - 1);
                    },
                    KeyCode::Home => state.cursor = 0,
                    KeyCode::End  => if total_rows > 0 { state.cursor = total_rows - 1; },
                    KeyCode::Char('h') => state.human_readable = !state.human_readable,
                    KeyCode::Char(c @ '1'..='9') => {
                        state.interval_secs = (c as u32) - ('0' as u32);
                    }
                    _ => {}
                }
            }
            while event::poll(Duration::ZERO)? { let _ = event::read()?; }
        }
    })
}

fn top(cli: Cli) -> Result<()> {

    let fs_arg = cli.filesystem.as_deref().unwrap_or(".");
    let handle = BcachefsHandle::open(fs_arg)
        .with_context(|| format!("opening filesystem '{}'", fs_arg))?;

    let delay = cli.delay.max(1);

    /* --once is shorthand for -n 1; if we're not on a TTY default to one
     * frame so piped output is sane. Otherwise count > 0 means N frames
     * and exit; count == 0 means run the interactive TUI. */
    let count = if cli.once { 1 }
                else if cli.count > 0 { cli.count }
                else if !io::stdout().is_terminal() { 1 }
                else { 0 };

    if count > 0 {
        run_non_interactive(&handle, cli.human_readable, count, delay)
    } else {
        run_interactive(handle, cli.human_readable, delay)
    }
}

pub const CMD: super::CmdDef = typed_cmd!("top", "Show live performance counters", Cli, top);
