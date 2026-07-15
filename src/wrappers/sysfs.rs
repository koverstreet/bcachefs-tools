use std::fs;
use std::io::{self, BufRead};
use std::os::fd::BorrowedFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Resolve the block device name for a bcachefs sysfs device directory.
///
/// Reads the `block` symlink at `dev_sysfs_path/block` (e.g.
/// `/sys/fs/bcachefs/<UUID>/dev-0/block`) and returns the basename
/// of the target (e.g. "sda"). Falls back to the directory name
/// (e.g. "dev-0") if the symlink is absent (offline device).
pub fn dev_name_from_sysfs(dev_sysfs_path: &Path) -> String {
    if let Ok(target) = fs::read_link(dev_sysfs_path.join("block")) {
        if let Some(name) = target.file_name() {
            return name.to_string_lossy().into_owned();
        }
    }
    dev_sysfs_path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn sysfs_path_from_fd(fd: BorrowedFd) -> Result<PathBuf> {
    use std::os::fd::AsRawFd;
    let raw = fd.as_raw_fd();
    let link = format!("/proc/self/fd/{}", raw);
    fs::read_link(&link).with_context(|| format!("resolving sysfs fd {}", raw))
}

/// Read a sysfs attribute as a u64.
pub fn read_sysfs_u64(path: &Path) -> io::Result<u64> {
    let s = fs::read_to_string(path)?;
    s.trim().parse::<u64>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
            format!("{}: {:?}", e, s.trim())))
}

/// Read a sysfs attribute as a string, relative to a directory fd.
pub fn read_sysfs_fd_str(dirfd: BorrowedFd, path: &str) -> io::Result<String> {
    let flags = rustix::fs::OFlags::RDONLY;
    let fd = rustix::fs::openat(dirfd, path, flags, rustix::fs::Mode::empty())?;
    let mut buf = [0u8; 256];
    let n = rustix::io::read(&fd, &mut buf)?;
    let s = std::str::from_utf8(&buf[..n])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(s.trim().to_string())
}


const KERNEL_VERSION_PATH: &str = "/sys/module/bcachefs/parameters/version";

/// Read the bcachefs kernel module metadata version.
/// Returns 0 if the module isn't loaded.
pub fn bcachefs_kernel_version() -> u64 {
    read_sysfs_u64(Path::new(KERNEL_VERSION_PATH)).unwrap_or(0)
}

/// Check if a block device is currently mounted.
///
/// Parses /proc/mounts and compares device identity (st_rdev for block
/// devices, st_dev+st_ino for files) against each mount's device path(s).
/// bcachefs mounts list multiple devices separated by colons.
pub fn dev_mounted(path: &str) -> bool {
    let Ok(d1) = fs::metadata(path) else { return false };

    let Ok(f) = fs::File::open("/proc/mounts") else { return false };
    for line in io::BufReader::new(f).lines() {
        let Ok(line) = line else { continue };
        let Some(dev_field) = line.split_whitespace().next() else { continue };

        for dev in dev_field.split(':') {
            let Ok(d2) = fs::metadata(dev) else { continue };

            let is_blk_1 = d1.file_type().is_block_device();
            let is_blk_2 = d2.file_type().is_block_device();
            if is_blk_1 != is_blk_2 {
                continue;
            }

            if is_blk_1 {
                if d1.rdev() == d2.rdev() {
                    return true;
                }
            } else if d1.dev() == d2.dev() && d1.ino() == d2.ino() {
                return true;
            }
        }
    }
    false
}

/// Write a string value to a sysfs attribute file relative to a directory fd.
pub fn sysfs_write_str(sysfs_fd: BorrowedFd, path: &str, value: &str) {
    let flags = rustix::fs::OFlags::WRONLY;
    if let Ok(fd) = rustix::fs::openat(sysfs_fd, path, flags, rustix::fs::Mode::empty()) {
        let _ = rustix::io::write(&fd, value.as_bytes());
    }
}

/// Info about a device in a mounted bcachefs filesystem, read from sysfs.
#[derive(Clone)]
pub struct DevInfo {
    pub idx:        u32,
    pub dev:        String,
    pub label:      Option<String>,
    pub durability: u32,
}

/// Enumerate devices for a mounted filesystem from its sysfs directory.
///
/// Reads `dev-N/` subdirectories under `sysfs_path`, extracting the block
/// device name (from the `block` symlink) for each.
pub fn fs_get_devices(sysfs_path: &Path) -> Result<Vec<DevInfo>> {
    let mut devs = Vec::new();
    for entry in fs::read_dir(sysfs_path)
        .with_context(|| format!("reading sysfs dir {}", sysfs_path.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let idx: u32 = match name.strip_prefix("dev-").and_then(|s| s.parse().ok()) {
            Some(i) => i,
            None => continue,
        };

        let dev_path = entry.path();
        let dev = dev_name_from_sysfs(&dev_path);

        let label = fs::read_to_string(dev_path.join("label"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let durability = read_sysfs_u64(&dev_path.join("durability"))
            .unwrap_or(1) as u32;

        devs.push(DevInfo { idx, dev, label, durability });
    }
    devs.sort_by_key(|d| d.idx);
    Ok(devs)
}
