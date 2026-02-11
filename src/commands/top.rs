use std::collections::HashMap;
use std::ffi::CStr;
use std::fs;
use std::io::{self, Write};
use std::mem;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use bch_bindgen::c::{bch_counters_flags, bch_ioctl_query_counters};
use bch_bindgen::c::bch_persistent_counters::BCH_COUNTER_NR;
use clap::Parser;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{self, ClearType},
};
use serde::Deserialize;

use crate::util::{fmt_bytes_human, fmt_num_human, run_tui};
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::ioctl::bch_ioc_w;
use crate::wrappers::sysfs::{dev_name_from_sysfs, sysfs_path_from_fd};

// ioctl constants

const BCH_IOCTL_QUERY_COUNTERS_NR: u32 = 21;
const BCH_IOCTL_QUERY_COUNTERS_MOUNT: u16 = 1 << 0;
const NR_COUNTERS: usize = BCH_COUNTER_NR as usize;

// Counter info accessors — read directly from bch_bindgen FFI arrays

fn counter_name(i: usize) -> &'static str {
    if i >= NR_COUNTERS { return "???" }
    let p = unsafe { *bch_bindgen::c::bch2_counter_names.as_ptr().add(i) };
    if p.is_null() { "???" } else { unsafe { CStr::from_ptr(p) }.to_str().unwrap_or("???") }
}

fn counter_stable_id(i: usize) -> u16 {
    if i >= NR_COUNTERS { return 0 }
    unsafe { *bch_bindgen::c::bch2_counter_stable_map.as_ptr().add(i) }
}

fn counter_is_sectors(i: usize) -> bool {
    if i >= NR_COUNTERS { return false }
    unsafe { *bch_bindgen::c::bch2_counter_flags_map.as_ptr().add(i) == bch_counters_flags::TYPE_SECTORS }
}

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

    /// Filesystem path, device, or UUID (default: current directory)
    filesystem: Option<String>,
}

// TUI state

struct TopState {
    ioctl_fd:       i32,
    nr_stable:      u16,
    mount_vals:     Vec<u64>,
    start_vals:     Vec<u64>,
    prev_vals:      Vec<u64>,
    prev_dev_io:    HashMap<String, (u64, u64)>,    // label -> (read, write)
    human_readable: bool,
    show_devices:   bool,
    sysfs_path:     PathBuf,
    interval_secs:  u32,
}

impl TopState {
    fn new(handle: &BcachefsHandle, human_readable: bool) -> Result<Self> {
        let ioctl_fd = handle.ioctl_fd_raw();
        let nr_stable = (0..NR_COUNTERS).map(|i| counter_stable_id(i)).max().unwrap_or(0) + 1;

        let mount_vals = read_counters(ioctl_fd, BCH_IOCTL_QUERY_COUNTERS_MOUNT, nr_stable)?;
        let start_vals = read_counters(ioctl_fd, 0, nr_stable)?;
        let prev_vals  = read_counters(ioctl_fd, 0, nr_stable)?;

        let sysfs_path = sysfs_path_from_fd(handle.sysfs_fd())?;

        Ok(TopState {
            ioctl_fd, nr_stable,
            mount_vals, start_vals, prev_vals,
            prev_dev_io: HashMap::new(),
            human_readable, show_devices: true,
            sysfs_path, interval_secs: 1,
        })
    }

    fn get_val(vals: &[u64], stable_id: u16) -> u64 {
        let idx = stable_id as usize;
        if idx < vals.len() { vals[idx] } else { 0 }
    }

    fn render(&self, curr: &[u64], dev_io: &[DevIoEntry], stdout: &mut io::Stdout) -> io::Result<()> {
        execute!(stdout, cursor::MoveTo(0, 0), terminal::Clear(ClearType::All))?;

        write!(stdout, "All counters have a corresponding tracepoint; for more info on any given event, try e.g.\r\n")?;
        write!(stdout, "  perf trace -e bcachefs:data_update_pred\r\n\r\n")?;
        write!(stdout, "  q:quit  h:human-readable  d:devices  1-9:interval\r\n\r\n")?;

        write!(stdout, "{:<40} {:>14} {:>14} {:>14}\r\n",
            "", format!("{}/s", self.interval_secs), "total", "mount")?;

        for i in 0..NR_COUNTERS {
            let (stable, sectors) = (counter_stable_id(i), counter_is_sectors(i));
            let cv = Self::get_val(curr, stable);
            let pv = Self::get_val(&self.prev_vals, stable);
            let sv = Self::get_val(&self.start_vals, stable);
            let mv = Self::get_val(&self.mount_vals, stable);

            let v_mount = cv.wrapping_sub(mv);
            if v_mount == 0 { continue }

            let v_rate  = cv.wrapping_sub(pv);
            let v_total = cv.wrapping_sub(sv);

            write!(stdout, "{:<40} {:>12}/s {:>14} {:>14}\r\n",
                counter_name(i),
                fmt_counter(v_rate / self.interval_secs as u64, sectors, self.human_readable),
                fmt_counter(v_total, sectors, self.human_readable),
                fmt_counter(v_mount, sectors, self.human_readable))?;
        }

        if self.show_devices && !dev_io.is_empty() {
            write!(stdout, "\r\nPer-device IO:\r\n")?;
            write!(stdout, "{:<40} {:>14} {:>14} {:>14} {:>14}\r\n",
                "", "read/s", "read", "write/s", "write")?;
            for dev in dev_io {
                let (prev_r, prev_w) = self.prev_dev_io
                    .get(&dev.label)
                    .copied()
                    .unwrap_or((dev.read_bytes, dev.write_bytes));
                let rate_r = dev.read_bytes.wrapping_sub(prev_r) / self.interval_secs as u64;
                let rate_w = dev.write_bytes.wrapping_sub(prev_w) / self.interval_secs as u64;

                let h = self.human_readable;
                write!(stdout, "{:<40} {:>14} {:>14} {:>14} {:>14}\r\n",
                    &dev.label,
                    fmt_bytes(rate_r, h), fmt_bytes(dev.read_bytes, h),
                    fmt_bytes(rate_w, h), fmt_bytes(dev.write_bytes, h))?;
            }
        }

        stdout.flush()
    }
}

fn run_interactive(handle: BcachefsHandle, human_readable: bool) -> Result<()> {
    let mut state = TopState::new(&handle, human_readable)?;

    run_tui(|stdout| loop {
        let curr = read_counters(state.ioctl_fd, 0, state.nr_stable)?;
        let dev_io = read_device_io(&state.sysfs_path);
        state.render(&curr, &dev_io, stdout)?;
        state.prev_vals = curr;
        state.prev_dev_io = dev_io.into_iter()
            .map(|d| (d.label, (d.read_bytes, d.write_bytes)))
            .collect();

        if event::poll(Duration::from_secs(state.interval_secs as u64))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                    KeyCode::Char('h') => state.human_readable = !state.human_readable,
                    KeyCode::Char('d') => state.show_devices = !state.show_devices,
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

pub fn top(argv: Vec<String>) -> Result<()> {
    let cli = Cli::parse_from(argv);

    let fs_arg = cli.filesystem.as_deref().unwrap_or(".");
    let handle = BcachefsHandle::open(fs_arg)
        .with_context(|| format!("opening filesystem '{}'", fs_arg))?;

    run_interactive(handle, cli.human_readable)
}
