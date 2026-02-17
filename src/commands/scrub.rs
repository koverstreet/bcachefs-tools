use std::io::{self, Read, Write};
use std::os::unix::io::FromRawFd;
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bch_bindgen::c::{
    bch_data_type, bch_ioctl_data, bch_ioctl_data_event_ret, bch_ioctl_data_progress,
};
use clap::Parser;

use crate::util::{fmt_bytes_human, fmt_sectors_human};
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::ioctl::bch_ioc_w;
use crate::wrappers::sysfs::{fs_get_devices, sysfs_path_from_fd};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn sigint_handler(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

const BCH_IOCTL_DATA_NR: u32 = 10;

/// bch_ioctl_data_event is blocklisted from bindgen (packed+aligned conflict),
/// so we read raw bytes and extract fields manually.
/// Layout: u8 type, u8 ret, u8 pad[6], bch_ioctl_data_progress, padding to 128.
const DATA_EVENT_SIZE: usize = 128;

fn read_data_event(fd: &mut std::fs::File) -> io::Result<(u8, u8, bch_ioctl_data_progress)> {
    let mut buf = [0u8; DATA_EVENT_SIZE];
    let n = fd.read(&mut buf)?;
    if n != DATA_EVENT_SIZE {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof,
            format!("short read from progress fd: {} bytes", n)));
    }
    let event_type = buf[0];
    let event_ret = buf[1];
    let p = unsafe {
        std::ptr::read_unaligned(buf.as_ptr().add(8) as *const bch_ioctl_data_progress)
    };
    Ok((event_type, event_ret, p))
}

fn start_scrub(ioctl_fd: i32, dev_idx: u32, data_types: u32) -> Result<std::fs::File> {
    let mut cmd = bch_ioctl_data {
        op: bch_bindgen::c::bch_data_ops::BCH_DATA_OP_scrub as u16,
        ..Default::default()
    };
    cmd.__bindgen_anon_1.scrub.dev = dev_idx;
    cmd.__bindgen_anon_1.scrub.data_types = data_types;

    let request = bch_ioc_w::<bch_ioctl_data>(BCH_IOCTL_DATA_NR);
    let ret = unsafe { libc::ioctl(ioctl_fd, request, &mut cmd as *mut bch_ioctl_data) };
    if ret < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { std::fs::File::from_raw_fd(ret) })
}

struct ScrubDev {
    name:           String,
    progress_fd:    Option<std::fs::File>,
    done:           u64,
    corrected:      u64,
    uncorrected:    u64,
    total:          u64,
    ret_status:     u8,
}

impl ScrubDev {
    fn format_line(&self, rate: u64) -> String {
        let pct = if self.total > 0 {
            format!("{}%", self.done * 100 / self.total)
        } else {
            "0%".to_string()
        };

        let status = if self.progress_fd.is_some() {
            format!("{}/sec", fmt_bytes_human(rate))
        } else if self.ret_status == bch_ioctl_data_event_ret::BCH_IOCTL_DATA_EVENT_RET_device_offline as u8 {
            "offline".to_string()
        } else {
            "complete".to_string()
        };

        format!("{:<16} {:>12} {:>12} {:>12} {:>12} {:>6}  {}",
            self.name,
            fmt_sectors_human(self.done),
            fmt_sectors_human(self.corrected),
            fmt_sectors_human(self.uncorrected),
            fmt_sectors_human(self.total),
            pct,
            status)
    }
}

#[derive(Parser, Debug)]
#[command(about = "Verify checksums and correct errors, if possible")]
pub struct Cli {
    /// Check metadata only
    #[arg(short, long)]
    metadata: bool,

    /// Filesystem path or device
    filesystem: String,
}

pub fn scrub(argv: Vec<String>) -> Result<()> {
    let cli = Cli::parse_from(argv);

    unsafe { libc::signal(libc::SIGINT, sigint_handler as libc::sighandler_t); }

    let data_types: u32 = if cli.metadata {
        1 << (bch_data_type::BCH_DATA_btree as u32)
    } else {
        !0u32
    };

    let handle = BcachefsHandle::open(&cli.filesystem)
        .with_context(|| format!("opening filesystem '{}'", cli.filesystem))?;

    let sysfs_path = sysfs_path_from_fd(handle.sysfs_fd())?;
    let devices = fs_get_devices(&sysfs_path)?;

    let ioctl_fd = handle.ioctl_fd_raw();
    let dev_idx = handle.dev_idx();

    let mut scrub_devs: Vec<ScrubDev> = Vec::new();

    if dev_idx >= 0 {
        let name = devices.iter()
            .find(|d| d.idx == dev_idx as u32)
            .map(|d| d.dev.clone())
            .unwrap_or_else(|| format!("dev-{}", dev_idx));

        let fd = start_scrub(ioctl_fd, dev_idx as u32, data_types)?;
        scrub_devs.push(ScrubDev {
            name, progress_fd: Some(fd),
            done: 0, corrected: 0, uncorrected: 0, total: 0, ret_status: 0,
        });
    } else {
        for dev in &devices {
            let fd = start_scrub(ioctl_fd, dev.idx, data_types)?;
            scrub_devs.push(ScrubDev {
                name: dev.dev.clone(), progress_fd: Some(fd),
                done: 0, corrected: 0, uncorrected: 0, total: 0, ret_status: 0,
            });
        }
    }

    let dev_names: Vec<&str> = scrub_devs.iter().map(|d| d.name.as_str()).collect();
    println!("Starting scrub on {} devices: {}",
        scrub_devs.len(), dev_names.join(" "));

    println!("{:<16} {:>12} {:>12} {:>12} {:>12} {:>6}",
        "device", "checked", "corrected", "uncorrected", "total", "");

    let mut exit_code = 0i32;
    let mut last = Instant::now();
    let mut first = true;

    loop {
        let now = Instant::now();
        let ns_elapsed = if first { 0u64 } else { (now - last).as_nanos() as u64 };

        let mut all_done = true;
        let mut lines: Vec<String> = Vec::new();

        for dev in &mut scrub_devs {
            let mut rate = 0u64;

            if let Some(ref mut fd) = dev.progress_fd {
                match read_data_event(fd) {
                    Ok((event_type, event_ret, p)) => {
                        // Skip non-progress events
                        if event_type != 0 {
                            all_done = false;
                            lines.push(dev.format_line(0));
                            continue;
                        }

                        if ns_elapsed > 0 {
                            rate = p.sectors_done.wrapping_sub(dev.done)
                                .checked_shl(9).unwrap_or(0)
                                .saturating_mul(1_000_000_000)
                                .checked_div(ns_elapsed).unwrap_or(0);
                        }

                        dev.done = p.sectors_done;
                        dev.corrected = p.sectors_error_corrected;
                        dev.uncorrected = p.sectors_error_uncorrected;
                        dev.total = p.sectors_total;

                        if dev.corrected > 0 { exit_code |= 2; }
                        if dev.uncorrected > 0 { exit_code |= 4; }

                        if event_ret != 0 {
                            dev.ret_status = event_ret;
                            dev.progress_fd = None;
                        }
                    }
                    Err(_) => {
                        dev.progress_fd = None;
                    }
                }
            }

            lines.push(dev.format_line(rate));

            if dev.progress_fd.is_some() {
                all_done = false;
            }
        }

        let stdout = io::stdout();
        let mut out = stdout.lock();

        if !first {
            for i in 0..scrub_devs.len() {
                if i > 0 { write!(out, "\x1b[1A")?; }
                write!(out, "\x1b[2K\r")?;
            }
        }

        for (i, line) in lines.iter().enumerate() {
            write!(out, "{}", line)?;
            if i < lines.len() - 1 { writeln!(out)?; }
        }
        out.flush()?;

        if all_done {
            writeln!(io::stdout())?;
            break;
        }

        if INTERRUPTED.load(Ordering::Relaxed) {
            writeln!(io::stdout())?;
            eprintln!("Interrupted");
            exit_code |= 1;
            break;
        }

        last = now;
        first = false;
        thread::sleep(Duration::from_secs(1));
    }

    if exit_code != 0 {
        process::exit(exit_code);
    }

    Ok(())
}
