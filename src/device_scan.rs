use std::{
    ffi::{CStr, CString, c_char, c_int, OsString, OsStr},
    collections::HashMap,
    fs,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
};

use anyhow::Result;
use bch_bindgen::{bcachefs, opt_get, opt_set};
use bcachefs::{
    bch_sb_handle,
    sb_name,
    sb_names,
};
use bcachefs::bch_opts;
use uuid::Uuid;
use log::debug;

fn read_super_silent(path: impl AsRef<Path>, mut opts: bch_opts) -> anyhow::Result<bch_sb_handle> {
    opt_set!(opts, noexcl, 1);
    opt_set!(opts, nochanges, 1);
    opt_set!(opts, no_version_check, 1);

    bch_bindgen::sb::io::read_super_silent(path.as_ref(), opts)
}

fn device_property_map(dev: &udev::Device) -> HashMap<String, String> {
    dev.properties()
        .map(|i| (
            i.name().to_string_lossy().into_owned(),
            i.value().to_string_lossy().into_owned(),
        ))
        .collect()
}

fn get_devices_by_uuid_udev(uuid: Uuid) -> anyhow::Result<Vec<String>> {
    debug!("Walking udev db!");

    let mut udev = udev::Enumerator::new()?;
    udev.match_subsystem("block")?;
    udev.match_property("ID_FS_TYPE", "bcachefs")?;

    let uuid = uuid.to_string();

    Ok(udev
        .scan_devices()?
        .filter(udev::Device::is_initialized)
        .map(|dev| device_property_map(&dev))
        .filter(|m|
            m.contains_key("ID_FS_UUID") &&
            m["ID_FS_UUID"] == uuid &&
            m.contains_key("DEVNAME"))
        .map(|m| m["DEVNAME"].clone())
        .collect::<Vec<_>>())
}

fn get_all_block_devnodes() -> anyhow::Result<Vec<String>> {
    let mut udev = udev::Enumerator::new()?;
    udev.match_subsystem("block")?;

    let devices = udev
        .scan_devices()?
        .filter_map(|dev| {
            if dev.is_initialized() {
                dev.devnode().map(|dn| dn.to_string_lossy().into_owned())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    Ok(devices)
}

fn read_sbs_matching_uuid(uuid: Uuid, devices: &[String], opts: &bch_opts) -> Vec<(PathBuf, bch_sb_handle)> {
    devices
        .iter()
        .filter_map(|dev| {
            read_super_silent(PathBuf::from(dev), *opts)
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
    let devs_from_udev = get_devices_by_uuid_udev(uuid)?;

    let devices = if use_udev && !devs_from_udev.is_empty() {
        devs_from_udev
    } else {
        get_all_block_devnodes()?
    };

    Ok(read_sbs_matching_uuid(uuid, &devices, opts))
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

    let dev_sb = read_super_silent(device, *opts)?;

    if dev_sb.sb().number_of_devices() == 1 {
        Ok(vec![(device.to_path_buf(), dev_sb)])
    } else {
        get_devices_by_uuid(dev_sb.sb().uuid(), opts, use_udev)
    }
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
            .map(|path|
                 bch_bindgen::sb::io::read_super_opts(path.as_ref(), opts)
                 .map(|sb| (path, sb)))
            .collect::<Result<Vec<_>>>()
    }

    let udev = opt_get!(opts, mount_trusts_udev) != 0;

    if let Some(("UUID" | "OLD_BLKID_UUID", uuid)) = device.split_once('=') {
        let uuid = Uuid::parse_str(uuid)?;

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

#[no_mangle]
pub extern "C" fn bch2_scan_device_sbs(device: *const c_char, ret: *mut sb_names) -> c_int {
    let device = unsafe { CStr::from_ptr(device) };
    let device = device.to_string_lossy().into_owned();

    // how to initialize to default/empty?
    let opts = bch_bindgen::opts::parse_mount_opts(None, None, true).unwrap_or_default();

    let mut sbs = scan_sbs(&device, &opts)
        .unwrap_or_else(|e| {
            eprintln!("bcachefs ({}): error reading superblock: {}", device, e);
            std::process::exit(-1);
        })
        .into_iter()
        .map(|(name, sb)| sb_name {
            name: CString::new(name.into_os_string().into_vec()).unwrap().into_raw(),
            sb: sb } )
        .collect::<Vec<sb_name>>();

    unsafe {
        (*ret).data   = sbs.as_mut_ptr();
        (*ret).nr     = sbs.len();
        (*ret).size   = sbs.capacity();
    }
    std::mem::forget(sbs);
    0
}

#[no_mangle]
pub extern "C" fn bch2_scan_devices(device: *const c_char) -> *mut c_char {
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
