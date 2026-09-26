use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::io::FromRawFd;
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bch_bindgen::c::{
    bch_ioctl_data, bch_ioctl_data_event_ret, bch_ioctl_data_progress,
    bch_ioctl_data__bindgen_ty_1__bindgen_ty_1 as ScrubArgs,
};
use bch_bindgen::accounting::data_type;
use clap::Parser;

use crate::commands::DeviceNameArgs;
use crate::util::{fmt_bytes_human, fmt_sectors_human};
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::ioctl::{ioctl_w, BCH_IOCTL_DATA};
use crate::wrappers::sysfs::{fs_get_devices, sysfs_path_from_fd};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn sigint_handler(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

const NO_PROGRESS_WARN_AFTER: Duration = Duration::from_secs(30);
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

fn start_scrub(ioctl_fd: std::os::fd::BorrowedFd, dev_idx: u32, data_types: u32) -> Result<std::fs::File> {
    let mut cmd = bch_ioctl_data {
        op: bch_bindgen::c::bch_data_ops::BCH_DATA_OP_scrub as u16,
        ..Default::default()
    };
    // bch_ioctl_data's op-params union is emitted as either a native Rust union or
    // the __BindgenUnionField wrapper, depending on the host libclang's Copy analysis
    // of its blocklisted __u32 members — non-deterministic across build hosts, and
    // the wrapper's helper type isn't nameable here. Both forms share one C layout,
    // so write the scrub params positionally; the asserts pin the layout we rely on.
    const _: () = assert!(std::mem::offset_of!(ScrubArgs, dev) == 0);
    const _: () = assert!(std::mem::offset_of!(ScrubArgs, data_types) == 4);
    unsafe {
        let p = std::ptr::addr_of_mut!(cmd.__bindgen_anon_1) as *mut u32;
        p.write(dev_idx);
        p.add(1).write(data_types);
    }

    let ret = ioctl_w::<BCH_IOCTL_DATA>(ioctl_fd, &cmd)?;
    Ok(unsafe { std::fs::File::from_raw_fd(ret) })
}

struct ScrubDev {
    name:              String,
    progress_fd:       Option<std::fs::File>,
    done:              u64,
    corrected:         u64,
    uncorrected:       u64,
    total:             u64,
    ret_status:        u8,
    /// Highest `done` value observed so far, and when it was last seen to
    /// advance. Used to detect a device whose progress ioctl keeps returning
    /// events but never reports more work done (koverstreet/bcachefs-tools#564).
    last_seen_done:    u64,
    last_seen_done_at: Instant,
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

/// True if every device that is still actively scrubbing (`progress_fd` is
/// still open) has not had its `done` counter advance for `warn_after`. This
/// catches both "the ioctl never returns" and the #564 case where progress
/// events keep arriving (so `total` is known and non-zero) but `done` never
/// moves off 0. A device with no `progress_fd` (finished, offline, or errored)
/// is not "active" and does not count either way.
fn scrub_stalled(devs: &[ScrubDev], now: Instant, warn_after: Duration) -> bool {
    let mut any_active = false;
    let mut all_stalled = true;
    for dev in devs {
        if dev.progress_fd.is_some() {
            any_active = true;
            if now.duration_since(dev.last_seen_done_at) < warn_after {
                all_stalled = false;
            }
        }
    }
    any_active && all_stalled
}

#[derive(Parser, Debug)]
#[command(about = "Verify checksums and correct errors, if possible")]
pub struct Cli {
    /// Check metadata only
    #[arg(short, long)]
    metadata: bool,

    #[command(flatten)]
    device_names: DeviceNameArgs,

    /// Filesystem path or device
    filesystem: String,
}

fn scrub(cli: Cli) -> Result<()> {

    unsafe {
        libc::signal(libc::SIGINT,
                     sigint_handler as extern "C" fn(libc::c_int) as libc::sighandler_t);
    }

    let data_types: u32 = if cli.metadata {
        1 << u32::from(data_type::btree)
    } else {
        !0u32
    };

    let handle = BcachefsHandle::open(&cli.filesystem)
        .with_context(|| format!("opening filesystem '{}'", cli.filesystem))?;

    let sysfs_path = sysfs_path_from_fd(handle.sysfs_fd())?;
    let name_mode = cli.device_names.name_mode();
    let devices = fs_get_devices(&sysfs_path, name_mode)?;

    let ioctl_fd = handle.ioctl_fd();
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
            last_seen_done: 0, last_seen_done_at: Instant::now(),
        });
    } else {
        for dev in &devices {
            let fd = start_scrub(ioctl_fd, dev.idx, data_types)?;
            scrub_devs.push(ScrubDev {
                name: dev.dev.clone(), progress_fd: Some(fd),
                done: 0, corrected: 0, uncorrected: 0, total: 0, ret_status: 0,
                last_seen_done: 0, last_seen_done_at: Instant::now(),
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
    let mut no_progress_warned = false;
    let mut first = true;
    let live_output = io::stdout().is_terminal();

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

                        if dev.done > dev.last_seen_done {
                            dev.last_seen_done = dev.done;
                            dev.last_seen_done_at = now;
                        }

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

        if scrub_stalled(&scrub_devs, now, NO_PROGRESS_WARN_AFTER) {
            if !no_progress_warned {
                writeln!(
                    io::stderr(),
                    "warning: scrub has not reported progress for {} seconds; \
                     check that the running kernel or DKMS module matches this bcachefs-tools version",
                    NO_PROGRESS_WARN_AFTER.as_secs()
                )?;
                no_progress_warned = true;
            }
        } else {
            no_progress_warned = false;
        }

        let interrupted = INTERRUPTED.load(Ordering::Relaxed);
        if live_output || all_done || interrupted {
            let stdout = io::stdout();
            let mut out = stdout.lock();

            if live_output && !first {
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
        }

        if all_done {
            writeln!(io::stdout())?;
            break;
        }

        if interrupted {
            writeln!(io::stdout())?;
            eprintln!("Interrupted");
            exit_code |= 1;

            // Parallelize kthread_stop() so we don't block on each thread serially
            let stops: Vec<_> = scrub_devs
                .iter_mut()
                .filter_map(|dev| dev.progress_fd.take())
                .map(|fd| thread::spawn(move || drop(fd)))
                .collect();
            for t in stops {
                let _ = t.join();
            }
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

pub const CMD: super::CmdDef = typed_cmd!(
    "scrub",
    "Verify data checksums; affected paths are logged to dmesg",
    Cli,
    scrub
);

#[cfg(test)]
mod tests {
    use super::*;

    /// A dummy fd for "this device is still actively scrubbing" — its
    /// content is never read by `scrub_stalled`, only `is_some()` matters.
    fn active_fd() -> Option<std::fs::File> {
        Some(std::fs::File::open("/dev/null").expect("open /dev/null"))
    }

    fn dev(progress_fd: Option<std::fs::File>, done: u64, last_seen_done_at: Instant) -> ScrubDev {
        ScrubDev {
            name: "test".to_string(),
            progress_fd,
            done,
            corrected: 0,
            uncorrected: 0,
            total: 1 << 20,
            ret_status: 0,
            last_seen_done: done,
            last_seen_done_at,
        }
    }

    // Negative control: a device that just made progress is never stalled,
    // no matter how small the warn threshold.
    #[test]
    fn immediate_progress_is_not_stalled() {
        let now = Instant::now();
        let devs = vec![dev(active_fd(), 100, now)];
        assert!(!scrub_stalled(&devs, now, Duration::from_secs(30)));
    }

    // The #564 case: total is known (so the old `total > 0` criterion would
    // have wrongly counted this as "progress"), but `done` has been stuck
    // since before the warn window opened.
    #[test]
    fn done_stuck_at_zero_with_known_total_is_stalled() {
        let now = Instant::now();
        let stuck_since = now - Duration::from_secs(31);
        let devs = vec![dev(active_fd(), 0, stuck_since)];
        assert!(scrub_stalled(&devs, now, Duration::from_secs(30)));
    }

    // Same stuck device, but not yet past the warn threshold.
    #[test]
    fn done_stuck_but_within_threshold_is_not_stalled() {
        let now = Instant::now();
        let stuck_since = now - Duration::from_secs(10);
        let devs = vec![dev(active_fd(), 0, stuck_since)];
        assert!(!scrub_stalled(&devs, now, Duration::from_secs(30)));
    }

    // A device that already finished (no progress_fd) does not count as
    // active, so it can't itself trigger or block the warning.
    #[test]
    fn finished_device_is_not_active() {
        let now = Instant::now();
        let devs = vec![dev(None, 100, now - Duration::from_secs(60))];
        assert!(!scrub_stalled(&devs, now, Duration::from_secs(30)));
    }

    // One device stuck, one still making real progress: the warning
    // requires *no* device to have advanced, so this must not fire.
    #[test]
    fn one_advancing_device_suppresses_the_warning() {
        let now = Instant::now();
        let devs = vec![
            dev(active_fd(), 0, now - Duration::from_secs(60)),
            dev(active_fd(), 100, now),
        ];
        assert!(!scrub_stalled(&devs, now, Duration::from_secs(30)));
    }
}
