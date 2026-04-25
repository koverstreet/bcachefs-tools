// device_scan: Discover bcachefs devices for mount.
//
// Multi-device bcachefs filesystems require all member devices to be
// identified before mounting. This module handles device discovery via
// two strategies:
//
// 1. **udev**: Query udev's database for devices tagged as bcachefs with
//    a matching UUID. Fast, but depends on udev having processed the device
//    — during early boot, devices may not be tagged yet.
//
// 2. **Block scan fallback**: Enumerate all block devices and read each
//    superblock directly. Slow but reliable. Used when udev returns fewer
//    devices than the superblock's nr_devices field indicates.
//
// The block scan falls back to /proc/partitions when udev is unavailable,
// so multi-device mount works without udevd running (#344). Remaining
// limitation: devices that haven't appeared yet are missed — the proper
// fix is event-driven waiting with a timeout. Related issues: #308, #393.
//
// The C FFI export bch2_scan_devices is called from cmd_fusemount.c.
// bch2_scan_device_sbs was removed — its only caller (bch2_sb_to_text_with_names)
// was rewritten in Rust (see wrappers/sb_display.rs) to fix an allocator
// mismatch where Vec-allocated memory was freed with kvfree.

use std::{
    ffi::{CStr, CString, c_char, OsString, OsStr},
    fs,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
};

use anyhow::Result;
use bch_bindgen::{bcachefs, opt_get, opt_set};
use bch_bindgen::errcode::BchError;
use bch_bindgen::fs::Fs;
use bcachefs::bch_sb_handle;
use bcachefs::bch_opts;
use uuid::Uuid;
use log::debug;

use crate::device_multipath::{find_multipath_holder, warn_multipath_component};

pub fn read_super_silent(path: impl AsRef<Path>, mut opts: bch_opts) -> Result<bch_sb_handle, BchError> {
    opt_set!(opts, noexcl, 1);
    opt_set!(opts, nochanges, 1);
    opt_set!(opts, no_version_check, 1);

    bch_bindgen::sb::io::read_super_silent(path.as_ref(), opts)
}

pub fn should_skip_multipath_component(dev: &udev::Device) -> bool {
    // Set by multipath's udev rule; fall back to sysfs if not present.
    if dev
        .property_value("DM_MULTIPATH_DEVICE_PATH")
        .is_some_and(|v| v == "1")
    {
        if let Some(devnode) = dev.devnode() {
            debug!("Skipping multipath component device: {}", devnode.display());
        }
        return true;
    }

    if let Some(devnode) = dev.devnode() {
        if find_multipath_holder(devnode).is_some() {
            debug!(
                "Skipping multipath component device via sysfs holders: {}",
                devnode.display()
            );
            return true;
        }
    }

    false
}

fn get_devices_by_uuid_udev(uuid: Uuid) -> anyhow::Result<Vec<PathBuf>> {
    debug!("Walking udev db!");

    let mut enumerator = udev::Enumerator::new()?;
    enumerator.match_is_initialized()?;
    enumerator.match_subsystem("block")?;
    enumerator.match_property("ID_FS_TYPE", "bcachefs")?;

    Ok(enumerator
        .scan_devices()?
        .filter(|dev| {
            dev.property_value("ID_FS_UUID")
                .and_then(OsStr::to_str)
                .and_then(|s| Uuid::parse_str(s).ok())
                .is_some_and(|dev_uuid| dev_uuid == uuid)
                && !should_skip_multipath_component(dev)
        })
        .filter_map(|dev| dev.devnode().map(Path::to_path_buf))
        .collect::<Vec<_>>())
}

fn get_all_block_devnodes_udev() -> anyhow::Result<Vec<PathBuf>> {
    let mut udev = udev::Enumerator::new()?;
    udev.match_is_initialized()?;
    udev.match_subsystem("block")?;

    let devices = udev
        .scan_devices()?
        .filter_map(|dev| dev.devnode().map(Path::to_path_buf))
        .collect::<Vec<_>>();
    Ok(devices)
}

/// Scan /proc/partitions for block devices. Works without udev.
fn get_all_block_devnodes_procfs() -> anyhow::Result<Vec<PathBuf>> {
    let contents = fs::read_to_string("/proc/partitions")?;
    let devices = contents
        .lines()
        .skip(2) // skip header lines
        .filter_map(|line| {
            let name = line.split_whitespace().nth(3)?;
            let path = Path::new("/dev").join(name);
            if path.exists() {
                Some(path.to_path_buf())
            } else {
                None
            }
        })
        .collect();
    Ok(devices)
}

fn get_all_block_devnodes() -> anyhow::Result<Vec<PathBuf>> {
    match get_all_block_devnodes_udev() {
        Ok(devs) if !devs.is_empty() => Ok(devs),
        Ok(_) => {
            debug!("udev returned no block devices, falling back to /proc/partitions");
            get_all_block_devnodes_procfs()
        }
        Err(e) => {
            debug!("udev block scan failed ({}), falling back to /proc/partitions", e);
            get_all_block_devnodes_procfs()
        }
    }
}

fn read_sbs_matching_uuid(
    uuid: Uuid,
    devices: &[PathBuf],
    opts: &bch_opts,
    filter_multipath: bool,
) -> Vec<(PathBuf, bch_sb_handle)> {
    devices
        .iter()
        .filter(|dev| {
            // When not using udev (which already filters), skip multipath components
            if filter_multipath && find_multipath_holder(dev).is_some() {
                debug!(
                    "Skipping multipath component device in fallback scan: {}",
                    dev.display()
                );
                return false;
            }
            true
        })
        .filter_map(|dev| {
            read_super_silent(dev, *opts)
                .ok()
                .map(|sb| (PathBuf::from(dev), sb))
        })
        .filter(|(_, sb)| sb.sb().uuid() == uuid)
        .collect::<Vec<_>>()
}

fn get_devices_by_uuid(
    uuid: Uuid,
    opts: &bch_opts,
    use_udev: bool
) -> anyhow::Result<Vec<(PathBuf, bch_sb_handle)>> {
    if use_udev {
        let devs_from_udev = get_devices_by_uuid_udev(uuid)?;

        if !devs_from_udev.is_empty() {
            let sbs = read_sbs_matching_uuid(uuid, &devs_from_udev, opts, false);

            // Check if udev found all expected devices. During early boot,
            // udev may not have finished processing all devices yet — if we
            // got fewer than expected, fall back to scanning all block devices.
            let expected = sbs.first()
                .map(|(_, sb)| sb.sb().number_of_devices() as usize)
                .unwrap_or(0);

            if sbs.len() >= expected {
                return Ok(sbs);
            }

            debug!("udev found {}/{} devices for UUID {}, falling back to block scan",
                sbs.len(), expected, uuid);
        }
    }

    // Falls back to /proc/partitions if udev is unavailable, so this works
    // without udevd running. Remaining TODO: wait for devices to appear
    // (poll or udev events) with a timeout, then attempt degraded mount.
    let all_devs = get_all_block_devnodes()?;
    Ok(read_sbs_matching_uuid(uuid, &all_devs, opts, true))
}

fn devs_str_sbs_from_device(
    device: &Path,
    opts: &bch_opts,
    use_udev: bool
) -> anyhow::Result<Vec<(PathBuf, bch_sb_handle)>> {
    if let Ok(metadata) = fs::metadata(device) {
        if metadata.is_dir() {
            return Err(anyhow::anyhow!("'{}' is a directory, not a block device", device.display()));
        }
    }

    // Honor explicit user-supplied paths, but warn when a path appears to be
    // a multipath component because that is typically unintended.
    if let Some(mpath_dev) = find_multipath_holder(device) {
        warn_multipath_component(device, &mpath_dev);
    }

    let dev_sb = read_super_silent(device, *opts)?;

    if dev_sb.sb().number_of_devices() == 1 {
        return Ok(vec![(device.to_path_buf(), dev_sb)]);
    }

    let uuid = dev_sb.sb().uuid();
    drop(dev_sb);

    get_devices_by_uuid(uuid, opts, use_udev)
}

pub fn parse_uuid_equals(s: &str) -> Result<Option<Uuid>> {
    let Some(("UUID" | "OLD_BLKID_UUID", uuid)) = s.split_once('=') else {
        return Ok(None);
    };
    Ok(Some(Uuid::parse_str(uuid)?))
}

pub fn scan_sbs(device: &String, opts: &bch_opts) -> Result<Vec<(PathBuf, bch_sb_handle)>> {
    if device.contains(':') {
        let mut opts = *opts;
        opt_set!(opts, noexcl, 1);
        opt_set!(opts, nochanges, 1);
        opt_set!(opts, no_version_check, 1);

        // If the device string contains ":" we will assume the user knows the
        // entire list. If they supply a single device it could be either the FS
        // only has 1 device or it's only 1 of a number of devices which are
        // part of the FS. This appears to be the case when we get called during
        // fstab mount processing and the fstab specifies a UUID.

        return device.split(':')
            .map(PathBuf::from)
            .map(|path| {
                if let Some(mpath_dev) = find_multipath_holder(path.as_path()) {
                    warn_multipath_component(path.as_path(), &mpath_dev);
                }

                bch_bindgen::sb::io::read_super_opts(path.as_ref(), opts)
                    .map(|sb| (path, sb))
            })
            .collect::<Result<Vec<_>>>()
    }

    let udev = opt_get!(opts, mount_trusts_udev) != 0;

    if let Some(uuid) = parse_uuid_equals(&device)? {
        get_devices_by_uuid(uuid, opts, udev)
    } else {
        devs_str_sbs_from_device(Path::new(device), opts, udev)
    }
}

pub fn joined_device_str(sbs: &[(PathBuf, bch_sb_handle)]) -> OsString {
    sbs.iter()
        .map(|sb| sb.0.clone().into_os_string())
        .collect::<Vec<_>>()
        .join(OsStr::new(":"))
}

pub fn scan_devices(device: &String, opts: &bch_opts) -> Result<OsString> {
    let sbs = scan_sbs(device, opts)?;

    Ok(joined_device_str(&sbs))
}

/// Discover all devices in a multi-device filesystem, then open it.
///
/// When `devs` contains a single device that belongs to a multi-device
/// filesystem, scans for the other members by UUID before opening —
/// same discovery that mount performs. When multiple devices are given
/// explicitly, passes them through as-is.
pub fn open_scan(devs: &[PathBuf], fs_opts: bch_opts) -> Result<Fs, BchError> {
    let devs = if devs.len() == 1 {
        let dev_str = devs[0].to_string_lossy().into_owned();
        let scan_opts = bch_bindgen::opts::parse_mount_opts(None, None, true)
            .unwrap_or_default();
        match scan_sbs(&dev_str, &scan_opts) {
            Ok(sbs) if !sbs.is_empty() => sbs.into_iter().map(|(p, _)| p).collect(),
            _ => devs.to_vec(),
        }
    } else {
        devs.to_vec()
    };

    Fs::open(&devs, fs_opts)
}

#[allow(dead_code)]
pub fn bch2_scan_devices(device: *const c_char) -> *mut c_char {
    let device = unsafe { CStr::from_ptr(device) };
    let device = device.to_string_lossy().into_owned();

    // how to initialize to default/empty?
    let opts = bch_bindgen::opts::parse_mount_opts(None, None, true).unwrap_or_default();

    let devs = scan_devices(&device, &opts).unwrap_or_else(|e| {
        eprintln!("bcachefs ({}): error reading superblock: {}", device, e);
        std::process::exit(-1);
    });

    CString::new(devs.into_vec()).unwrap().into_raw()
}
