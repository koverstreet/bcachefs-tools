use std::{
    ffi::{CStr, CString, c_char, c_int},
    collections::HashMap,
    env,
    fs,
    path::{Path, PathBuf},
    str,
};

use anyhow::Result;
use bch_bindgen::{bcachefs, opt_set};
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
    opt_set!(opts, no_version_check, 1);

    bch_bindgen::sb_io::read_super_silent(path.as_ref(), opts)
}

fn device_property_map(dev: &udev::Device) -> HashMap<String, String> {
    let rc: HashMap<_, _> = dev
        .properties()
        .map(|i| {
            (
                String::from(i.name().to_string_lossy()),
                String::from(i.value().to_string_lossy()),
            )
        })
        .collect();
    rc
}

fn udev_bcachefs_info() -> anyhow::Result<HashMap<String, Vec<String>>> {
    let mut info = HashMap::new();

    if env::var("BCACHEFS_BLOCK_SCAN").is_ok() {
        debug!("Checking all block devices for bcachefs super block!");
        return Ok(info);
    }

    let mut udev = udev::Enumerator::new()?;

    debug!("Walking udev db!");

    udev.match_subsystem("block")?;
    udev.match_property("ID_FS_TYPE", "bcachefs")?;

    for m in udev
        .scan_devices()?
        .filter(udev::Device::is_initialized)
        .map(|dev| device_property_map(&dev))
        .filter(|m| m.contains_key("ID_FS_UUID") && m.contains_key("DEVNAME"))
    {
        let fs_uuid = m["ID_FS_UUID"].clone();
        let dev_node = m["DEVNAME"].clone();
        info.insert(dev_node.clone(), vec![fs_uuid.clone()]);
        info.entry(fs_uuid).or_insert(vec![]).push(dev_node.clone());
    }

    Ok(info)
}

fn get_super_blocks(uuid: Uuid, devices: &[String], opts: &bch_opts) -> Vec<(PathBuf, bch_sb_handle)> {
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

fn get_devices_by_uuid(
    udev_bcachefs: &HashMap<String, Vec<String>>,
    uuid: Uuid,
    opts: &bch_opts
) -> anyhow::Result<Vec<(PathBuf, bch_sb_handle)>> {
    let devices = {
        if !udev_bcachefs.is_empty() {
            let uuid_string = uuid.hyphenated().to_string();
            if let Some(devices) = udev_bcachefs.get(&uuid_string) {
                devices.clone()
            } else {
                Vec::new()
            }
        } else {
            get_all_block_devnodes()?
        }
    };

    Ok(get_super_blocks(uuid, &devices, opts))
}

fn devs_str_sbs_from_uuid(
    udev_info: &HashMap<String, Vec<String>>,
    uuid: &str,
    opts: &bch_opts
) -> Result<Vec<(PathBuf, bch_sb_handle)>> {
    debug!("enumerating devices with UUID {}", uuid);

    Uuid::parse_str(uuid).map(|uuid| get_devices_by_uuid(udev_info, uuid, opts))?
}

fn devs_str_sbs_from_device(
    udev_info: &HashMap<String, Vec<String>>,
    device: &Path,
    opts: &bch_opts
) -> anyhow::Result<Vec<(PathBuf, bch_sb_handle)>> {
    if let Ok(metadata) = fs::metadata(device) {
        if metadata.is_dir() {
            return Err(anyhow::anyhow!("'{}' is a directory, not a block device", device.display()));
        }
        if metadata.is_file() {
            return Err(anyhow::anyhow!("'{}' is a regular file, not a block device", device.display()));
        }
    }

    let dev_sb = read_super_silent(device, *opts)?;

    if dev_sb.sb().number_of_devices() == 1 {
        Ok(vec![(device.to_path_buf(), dev_sb)])
    } else {
        let uuid = dev_sb.sb().uuid();

        get_devices_by_uuid(udev_info, uuid, opts)
    }
}

pub fn scan_sbs(device: &String, opts: &bch_opts) -> Result<Vec<(PathBuf, bch_sb_handle)>> {
    // Grab the udev information once
    let udev_info = udev_bcachefs_info()?;

    if let Some(("UUID" | "OLD_BLKID_UUID", uuid)) = device.split_once('=') {
        devs_str_sbs_from_uuid(&udev_info, uuid, opts)
    } else if device.contains(':') {
        // If the device string contains ":" we will assume the user knows the
        // entire list. If they supply a single device it could be either the FS
        // only has 1 device or it's only 1 of a number of devices which are
        // part of the FS. This appears to be the case when we get called during
        // fstab mount processing and the fstab specifies a UUID.

        let devices = device.split(':')
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        let sbs = devices
            .iter()
            .map(|path| read_super_silent(path, *opts))
            .collect::<Result<Vec<_>>>()?;

        Ok(devices.iter().zip(sbs.iter())
            .map(|(x, y)| (PathBuf::from(x), y.clone()))
            .collect::<Vec<_>>())
    } else {
        devs_str_sbs_from_device(&udev_info, Path::new(device), opts)
    }
}

pub fn joined_device_str(sbs: &Vec<(PathBuf, bch_sb_handle)>) -> String {
    sbs.iter()
        .map(|sb| sb.0.clone().into_os_string().into_string().unwrap())
        .collect::<Vec<_>>()
        .join(":")
}

pub fn scan_devices(device: &String, opts: &bch_opts) -> Result<String> {
    let mut sbs = scan_sbs(device, opts)?;

    for sb in &mut sbs {
        unsafe {
            bch_bindgen::sb_io::bch2_free_super(&mut sb.1);
        }
    }

    Ok(joined_device_str(&sbs))
}

#[no_mangle]
pub extern "C" fn bch2_scan_device_sbs(device: *const c_char, ret: *mut sb_names) -> c_int {
    let device = unsafe { CStr::from_ptr(device) };
    let device = device.to_str().unwrap().to_string();

    // how to initialize to default/empty?
    let opts = bch_bindgen::opts::parse_mount_opts(None, None, true).unwrap_or_default();

    let sbs = match scan_sbs(&device, &opts) {
        Ok(sbs) => sbs,
        Err(e) => {
            eprintln!("bcachefs ({}): error reading superblock: {}", device, e);
            return -1;
        }
    };

    let mut sbs = sbs.iter()
        .map(|(name, sb)| sb_name {
            name: CString::new(name.clone().into_os_string().into_string().unwrap()).unwrap().into_raw(),
            sb: *sb } )
        .collect::<Vec<sb_name>>();

    unsafe {
        (*ret).data   = sbs.as_mut_ptr();
        (*ret).nr     = sbs.len();
        (*ret).size   = sbs.capacity();

        std::mem::forget(sbs);
    }
    0
}

#[no_mangle]
pub extern "C" fn bch2_scan_devices(device: *const c_char) -> *mut c_char {
    let device = unsafe { CStr::from_ptr(device) };
    let device = device.to_str().unwrap().to_string();

    // how to initialize to default/empty?
    let opts = bch_bindgen::opts::parse_mount_opts(None, None, true).unwrap_or_default();

    match scan_devices(&device, &opts) {
        Ok(result) => CString::new(result).unwrap().into_raw(),
        Err(e) => {
            eprintln!("bcachefs ({}): error reading superblock: {}", device, e);
            std::ptr::null_mut()
        }
    }
}
