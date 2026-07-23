use std::{
    collections::HashSet,
    ffi::OsStr,
    os::fd::{AsRawFd, BorrowedFd},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::bail;
use bcachefs_kernel::c;
use c::bch_sb_handle;
use clap::Parser;
use log::{debug, info, warn};
use rustix::event::{poll, PollFd, PollFlags, Timespec};
use uuid::Uuid;

use crate::{device_scan, logging};

/// Waits until every device in a filesystem is initialized.
#[derive(Parser, Debug)]
#[command(
    about,
    long_about = "Waits until every device in a filesystem is initialized. \
udev is used to scan for devices and be notified of device changes. A zero \
exit status means that every device was initialized at some point. A non-zero \
exit status means that an error was encountered."
)]
pub struct Cli {
    /// A device string in the UUID=\<UUID\> format.
    device: String,

    /// Maximum seconds to wait. The default is to wait forever.
    #[arg(long)]
    timeout: Option<u64>,

    /// Be verbose. Can be specified more than once.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

fn cmd_wait_devices(cli: Cli) -> anyhow::Result<()> {
    logging::setup(cli.verbose, false);

    let Some(uuid) = device_scan::parse_uuid_equals(&cli.device)? else {
        bail!("invalid device string: {}", cli.device);
    };

    let opts = c::bch_opts::default();
    if !wait_for_devices(uuid, cli.timeout.map(Duration::from_secs), &opts)? {
        bail!("timed out waiting for all devices for UUID={uuid}");
    }

    Ok(())
}

pub fn wait_for_devices(
    uuid: Uuid,
    timeout: Option<Duration>,
    opts: &c::bch_opts,
) -> anyhow::Result<bool> {
    let mut wait_initialized = WaitInitialized::new(uuid, *opts);

    let socket = udev::MonitorBuilder::new()?
        .match_subsystem("block")?
        .listen()?;

    let mut enumerator = udev::Enumerator::new()?;
    enumerator.match_is_initialized()?;
    enumerator.match_subsystem("block")?;
    enumerator.match_property("ID_FS_TYPE", "bcachefs")?;

    for device in enumerator.scan_devices()? {
        let Some(devnode) = device.devnode() else {
            continue;
        };
        wait_initialized.add(devnode, &device)?;
    }

    while !wait_initialized.every_device_is_initialized()? {
        let poll_timeout = timeout
            .and_then(|timeout| deadline_remaining(wait_initialized.started_at, timeout))
            .map(duration_to_timespec);
        if timeout.is_some() && poll_timeout.is_none() {
            return Ok(false);
        }

        let socket_fd = unsafe { BorrowedFd::borrow_raw(socket.as_raw_fd()) };

        let mut fds = [PollFd::new(&socket_fd, PollFlags::IN)];
        let event_count = poll(&mut fds, poll_timeout.as_ref())?;
        if event_count == 0 {
            return Ok(false);
        }
        if fds.iter().any(|fd| fd.revents().contains(PollFlags::ERR)) {
            bail!("error on udev socket fd");
        }

        wait_initialized.process_events(&socket)?;
    }

    Ok(true)
}

fn deadline_remaining(started_at: Instant, timeout: Duration) -> Option<Duration> {
    timeout.checked_sub(started_at.elapsed())
}

fn duration_to_timespec(duration: Duration) -> Timespec {
    Timespec {
        tv_sec: duration.as_secs().try_into().unwrap_or(i64::MAX),
        tv_nsec: duration.subsec_nanos().into(),
    }
}

struct WaitInitialized {
    uuid: Uuid,
    opts: c::bch_opts,
    sbs: Vec<(PathBuf, bch_sb_handle)>,
    started_at: Instant,
}

impl WaitInitialized {
    fn new(uuid: Uuid, opts: c::bch_opts) -> Self {
        WaitInitialized {
            uuid,
            opts,
            sbs: Vec::new(),
            started_at: Instant::now(),
        }
    }

    fn add(&mut self, devnode: &Path, device: &udev::Device) -> anyhow::Result<()> {
        if !device.is_initialized()
            || device
                .property_value("ID_FS_TYPE")
                .is_none_or(|fs_type| fs_type != "bcachefs")
            || device
                .property_value("ID_FS_UUID")
                .and_then(OsStr::to_str)
                .and_then(|s| Uuid::parse_str(s).ok())
                .is_some_and(|device_uuid| device_uuid != self.uuid)
        {
            return Ok(());
        }
        if device_scan::should_skip_multipath_component(device) {
            return Ok(());
        }
        let sb_handle = match device_scan::read_super_silent(devnode, self.opts) {
            Ok(handle) => handle,
            Err(err) if err.raw() == libc::ENOENT => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        let sb = sb_handle.sb();
        if sb.uuid() != self.uuid {
            return Ok(());
        }
        let dev_idx = sb.dev_idx;
        if u32::from(dev_idx) >= sb.number_of_devices() {
            warn!(
                "superblock with invalid dev_idx: {dev_idx} >= {}",
                sb.number_of_devices()
            );
            return Ok(());
        }

        debug!(
            "adding device at {} with index {dev_idx}",
            devnode.display()
        );
        self.sbs.push((devnode.to_path_buf(), sb_handle));
        Ok(())
    }

    fn remove(&mut self, devnode: &Path) {
        if let Some(i) = self.sbs.iter().position(|(dev, _)| dev == devnode) {
            let dev_idx = self.sbs[i].1.sb().dev_idx;
            self.sbs.remove(i);
            debug!(
                "removing device at {} with index {dev_idx}",
                devnode.display()
            );
        }
    }

    fn process_events(&mut self, socket: &udev::MonitorSocket) -> anyhow::Result<()> {
        for event in socket.iter() {
            debug!("udev event: {event:?}");
            let Some(devnode) = event.devnode() else {
                continue;
            };
            let add = match event.event_type() {
                udev::EventType::Add | udev::EventType::Change => true,
                udev::EventType::Remove => false,
                _ => continue,
            };
            self.remove(devnode);
            if add {
                self.add(devnode, &event.device())?;
            }
        }
        Ok(())
    }

    fn every_device_is_initialized(&mut self) -> anyhow::Result<bool> {
        self.sbs = device_scan::filter_current_sbs(std::mem::take(&mut self.sbs), &self.opts)?;

        let Some((_, best)) = self.sbs.first() else {
            return Ok(false);
        };

        let number_of_devices = best.sb().number_of_devices() as usize;
        let unique_dev_indices: HashSet<u8> =
            self.sbs.iter().map(|(_, sb)| sb.sb().dev_idx).collect();

        info!(
            "found {}/{} initialized devices for UUID={}",
            unique_dev_indices.len(),
            number_of_devices,
            self.uuid
        );

        Ok(unique_dev_indices.len() == number_of_devices)
    }
}

pub const CMD: super::CmdDef = typed_cmd!(
    "wait-devices",
    "Wait until every device in a filesystem is initialized",
    Cli,
    cmd_wait_devices
);
