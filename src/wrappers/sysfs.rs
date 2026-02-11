use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::BorrowedFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bch_bindgen::c;

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

pub fn sysfs_path_from_fd(fd: i32) -> Result<PathBuf> {
    let link = format!("/proc/self/fd/{}", fd);
    fs::read_link(&link).with_context(|| format!("resolving sysfs fd {}", fd))
}

/// Read a sysfs attribute as a u64.
pub fn read_sysfs_u64(path: &Path) -> io::Result<u64> {
    let s = fs::read_to_string(path)?;
    s.trim().parse::<u64>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
            format!("{}: {:?}", e, s.trim())))
}

/// Read a sysfs attribute as a u64, relative to a directory fd.
pub fn read_sysfs_fd_u64(dirfd: i32, path: &str) -> io::Result<u64> {
    let dir = unsafe { BorrowedFd::borrow_raw(dirfd) };
    let flags = rustix::fs::OFlags::RDONLY;
    let fd = rustix::fs::openat(dir, path, flags, rustix::fs::Mode::empty())?;
    let mut buf = [0u8; 64];
    let n = rustix::io::read(&fd, &mut buf)?;
    let s = std::str::from_utf8(&buf[..n])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    s.trim().parse::<u64>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

const KERNEL_VERSION_PATH: &str = "/sys/module/bcachefs/parameters/version";

/// Read the bcachefs kernel module metadata version.
/// Returns 0 if the module isn't loaded.
pub fn bcachefs_kernel_version() -> u64 {
    read_sysfs_u64(Path::new(KERNEL_VERSION_PATH)).unwrap_or(0)
}

/// Check if a block device is currently mounted as a bcachefs member.
pub fn dev_mounted(path: &str) -> bool {
    let Ok(c_path) = CString::new(path) else { return false };
    unsafe { c::dev_mounted(c_path.as_ptr()) != 0 }
}

/// Write a string value to a sysfs attribute file relative to a directory fd.
pub fn sysfs_write_str(sysfs_fd: i32, path: &str, value: &str) {
    let dir = unsafe { BorrowedFd::borrow_raw(sysfs_fd) };
    let flags = rustix::fs::OFlags::WRONLY;
    if let Ok(fd) = rustix::fs::openat(dir, path, flags, rustix::fs::Mode::empty()) {
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
