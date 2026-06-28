// SPDX-License-Identifier: GPL-2.0

//! Block device utilities.
//!
//! Pure Rust replacements for get_size(), get_blocksize(), fd_to_dev_model()
//! from tools-util.c. These work on any fd (block device or regular file).

use std::fs::Metadata;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::io::RawFd;
use std::path::PathBuf;

use libc::BLKPBSZGET;

// linux/fs.h ioctl constants not exposed by libc crate
const BLKGETSIZE64: libc::Ioctl = 0x80081272u32 as libc::Ioctl;

/// Returns the size of a file or block device in bytes.
///
/// For block devices, uses BLKGETSIZE64 ioctl.
/// For regular files, returns st_size from fstat.
pub fn get_size(fd: RawFd) -> u64 {
    let metadata = fd_metadata(fd);

    if metadata.file_type().is_block_device() {
        let mut size: u64 = 0;
        unsafe { libc::ioctl(fd, BLKGETSIZE64, &mut size) };
        size
    } else {
        metadata.size()
    }
}

/// Returns the physical block size of a block device (the _larger_ of the two),
/// with fallback to the filesystem block size hint for regular files, in bytes.
/// (to be used as a performance hint only)
pub fn get_blocksize_physical_hint(fd: RawFd) -> u32 {
    let metadata = fd_metadata(fd);

    if metadata.file_type().is_block_device() {
        let mut bs: libc::c_uint = 0;
        unsafe { libc::ioctl(fd, BLKPBSZGET, &mut bs) };
        bs
    } else {
        metadata.blksize() as u32
    }
}

/// Returns the device model string for a block device fd, or a
/// fallback description for regular files / unknown devices.
pub fn fd_to_dev_model(fd: RawFd) -> String {
    let metadata = fd_metadata(fd);

    if !metadata.file_type().is_block_device() {
        return "(image file)".to_string();
    }

    let major = rustix::fs::major(metadata.rdev());
    let minor = rustix::fs::minor(metadata.rdev());
    let sysfs = format!("/sys/dev/block/{}:{}", major, minor);

    // Try device/model, then parent's device/model (partition),
    // then loop/backing_file
    for suffix in &["device/model", "../device/model", "loop/backing_file"] {
        let path = format!("{}/{}", sysfs, suffix);
        if let Ok(contents) = std::fs::read_to_string(&path) {
            let trimmed = contents.trim_end_matches('\n');
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }

    "(unknown model)".to_string()
}

/// Returns the device serial number for a block device fd, or None
/// if not available (image files, devices without serial sysfs entry).
pub fn fd_to_dev_serial(fd: RawFd) -> Option<String> {
    let metadata = fd_metadata(fd);

    if !metadata.file_type().is_block_device() {
        return None;
    }

    let major = rustix::fs::major(metadata.rdev());
    let minor = rustix::fs::minor(metadata.rdev());
    let sysfs = format!("/sys/dev/block/{}:{}", major, minor);

    // Try device/serial, then parent's device/serial (partition)
    for suffix in &["device/serial", "../device/serial"] {
        let path = format!("{}/{}", sysfs, suffix);
        if let Ok(contents) = std::fs::read_to_string(&path) {
            let trimmed = contents.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }

    None
}

/// Returns a stable sysfs identity for the whole disk backing this block
/// device. Partitions are resolved to their parent disk.
pub fn fd_to_parent_disk_sysfs(fd: RawFd) -> Option<PathBuf> {
    let metadata = fd_metadata(fd);

    if !metadata.file_type().is_block_device() {
        return None;
    }

    let major = rustix::fs::major(metadata.rdev());
    let minor = rustix::fs::minor(metadata.rdev());
    let sysfs = PathBuf::from(format!("/sys/dev/block/{}:{}", major, minor));
    let path = std::fs::canonicalize(sysfs).ok()?;

    if path.join("partition").exists() {
        path.parent().map(PathBuf::from)
    } else {
        Some(path)
    }
}

/// Returns true if the block device is non-rotational (SSD).
///
/// For regular files (image files) the ioctl fails and we default to false.
pub fn nonrot(fd: RawFd) -> bool {
    // BLKROTATIONAL = _IO(0x12, 126). Kernel returns !bdev_nonrot(bdev) via
    // put_ushort — bdev_nonrot internally uses bdev_get_queue(bdev), which
    // resolves to the parent disk's queue for partitions and handles LVM,
    // md, loop devices, etc. uniformly.
    const BLKROTATIONAL: libc::Ioctl = 0x127E as libc::Ioctl;
    let mut rotational: u16 = 0;
    let ret = unsafe { libc::ioctl(fd, BLKROTATIONAL, &mut rotational) };
    ret == 0 && rotational == 0
}

// BLK_OPEN_* flags from include/linux/blk_types.h
pub const BLK_OPEN_READ: u32     = 1 << 0;
pub const BLK_OPEN_WRITE: u32    = 1 << 1;
pub const BLK_OPEN_EXCL: u32     = 1 << 2;
pub const BLK_OPEN_BUFFERED: u32 = 1 << 5;
pub const BLK_OPEN_CREAT: u32    = 1 << 6;

/// Open a block device or file for formatting.
///
/// Translates BLK_OPEN_* flags to POSIX open flags.
/// Returns the owned fd on success, or a negative errno on failure.
pub fn open_device(path: &std::ffi::CStr, mode: u32) -> Result<OwnedFd, i32> {
    let mut flags = rustix::fs::OFlags::empty();

    let rw = mode & (BLK_OPEN_READ | BLK_OPEN_WRITE);
    if rw == (BLK_OPEN_READ | BLK_OPEN_WRITE) {
        flags = rustix::fs::OFlags::RDWR;
    } else if mode & BLK_OPEN_READ != 0 {
        flags = rustix::fs::OFlags::RDONLY;
    } else if mode & BLK_OPEN_WRITE != 0 {
        flags = rustix::fs::OFlags::WRONLY;
    }

    if mode & BLK_OPEN_BUFFERED == 0 {
        flags |= rustix::fs::OFlags::DIRECT;
    }

    if mode & BLK_OPEN_EXCL != 0 {
        flags |= rustix::fs::OFlags::EXCL;
    }

    if mode & BLK_OPEN_CREAT != 0 {
        flags |= rustix::fs::OFlags::CREATE;
    }

    rustix::fs::open(path, flags, rustix::fs::Mode::from_raw_mode(0o600))
        .map_err(|e| e.raw_os_error())
}

/// Call the C blkid_check function to probe for existing filesystems.
pub fn blkid_check(fd: RawFd, path: &std::ffi::CStr, force: bool) {
    extern "C" {
        fn blkid_check(fd: libc::c_int, path: *const libc::c_char, force: bool);
    }
    unsafe { blkid_check(fd, path.as_ptr(), force) }
}

fn fd_metadata(fd: RawFd) -> Metadata {
    match std::fs::metadata(format!("/proc/self/fd/{}", fd)) {
        Ok(metadata) => metadata,
        Err(_) => {
            crate::wrappers::super_io::die("stat error");
        }
    }
}
