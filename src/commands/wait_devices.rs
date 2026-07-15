use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    os::fd::{AsRawFd, BorrowedFd},
    path::{Path, PathBuf},
};

use anyhow::bail;
use bch_bindgen::bcachefs;
use clap::Parser;
use log::{debug, warn};
use rustix::event::{poll, PollFd, PollFlags};
use uuid::Uuid;

use crate::device_scan;

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
}

fn cmd_wait_devices(cli: Cli) -> anyhow::Result<()> {
    let Some(uuid) = device_scan::parse_uuid_equals(&cli.device)? else {
        bail!("invalid device string: {}", cli.device);
    };

    let mut wait_initialized = WaitInitialized::new(uuid);

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

    while !wait_initialized.every_device_is_initialized() {
        let socket_fd = unsafe { BorrowedFd::borrow_raw(socket.as_raw_fd()) };

        let mut fds = [PollFd::new(&socket_fd, PollFlags::IN)];
        poll(&mut fds, -1)?;
        if fds.iter().any(|fd| fd.revents().contains(PollFlags::ERR)) {
            bail!("error on udev socket fd");
        }

        wait_initialized.process_events(&socket)?;
    }

    Ok(())
}

struct WaitInitialized {
    uuid:               Uuid,
    number_of_devices:  Option<u32>,
    dev_idx_by_devnode: HashMap<PathBuf, u8>,
}

impl WaitInitialized {
    fn new(uuid: Uuid) -> Self {
        WaitInitialized {
            uuid,
            number_of_devices: None,
            dev_idx_by_devnode: HashMap::new(),
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
        let opts = bcachefs::bch_opts::default();
        let sb_handle = match device_scan::read_super_silent(devnode, opts) {
            Ok(handle) => handle,
            Err(err) if err.raw() == libc::ENOENT => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        let sb = sb_handle.sb();
        if sb.uuid() != self.uuid {
            return Ok(());
        }
        let dev_idx = sb.dev_idx;
        let number_of_devices = sb.number_of_devices();
        if u32::from(dev_idx) >= number_of_devices {
            warn!("superblock with invalid dev_idx: {dev_idx} >= {number_of_devices}");
            return Ok(());
        }
        if let Some(n) = self.number_of_devices {
            if n != number_of_devices {
                bail!("inconsistent number of devices: {n} != {number_of_devices}");
            }
        } else {
            self.number_of_devices = Some(number_of_devices);
        }
        debug!(
            "adding device at {} with index {dev_idx}",
            devnode.display()
        );
        self.dev_idx_by_devnode
            .insert(devnode.to_path_buf(), dev_idx);
        Ok(())
    }

    fn remove(&mut self, devnode: &Path) {
        if let Some(dev_idx) = self.dev_idx_by_devnode.remove(devnode) {
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

    fn every_device_is_initialized(&self) -> bool {
        let Some(number_of_devices) = self.number_of_devices.and_then(|n| usize::try_from(n).ok())
        else {
            return false;
        };
        let unique_dev_indices: HashSet<u8> = self.dev_idx_by_devnode.values().copied().collect();
        unique_dev_indices.len() == number_of_devices
    }
}

pub const CMD: super::CmdDef =
    typed_cmd!("wait-devices", "Wait until every device in a filesystem is initialized", Cli, cmd_wait_devices);
