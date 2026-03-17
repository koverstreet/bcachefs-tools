// SPDX-License-Identifier: GPL-2.0

//! Block device utilities.
//!
//! Pure Rust replacements for get_size(), get_blocksize(), fd_to_dev_model()
//! from tools-util.c. These work on any fd (block device or regular file).

use std::os::unix::io::RawFd;

use libc::{
    BLKPBSZGET,
    BLKSSZGET,
};

// linux/fs.h ioctl constants not exposed by libc crate
const BLKGETSIZE64: libc::c_ulong = 0x80081272;

/// Returns the size of a file or block device in bytes.
///
/// For block devices, uses BLKGETSIZE64 ioctl.
/// For regular files, returns st_size from fstat.
pub fn get_size(fd: RawFd) -> u64 {
    let stat = fstat(fd);

    if is_blk(stat.st_mode) {
        let mut size: u64 = 0;
        unsafe { libc::ioctl(fd, BLKGETSIZE64, &mut size) };
        size
    } else {
        stat.st_size as u64
    }
}

/// Returns the physical block size of a block device (the _larger_ of the two),
/// with fallback to the filesystem block size hint for regular files, in bytes.
/// (to be used as a performance hint only)
pub fn get_blocksize_physical_hint(fd: RawFd) -> u32 {
    let stat = fstat(fd);

    if is_blk(stat.st_mode) {
        let mut bs: libc::c_uint = 0;
        unsafe { libc::ioctl(fd, BLKPBSZGET, &mut bs) };
        bs
    } else {
        stat.st_blksize as u32
    }
}

/// Returns logical block size (LBA) of a block device (the smaller of the two),
/// with fallback to the filesystem block size hint for regular files, in bytes.
/// (suitable for use as an alignment for direct I/O or similar)
pub fn get_blocksize_logical(fd: RawFd) -> u32 {
    let stat = fstat(fd);

    if is_blk(stat.st_mode) {
        let mut bs: libc::c_uint = 0;
        unsafe { libc::ioctl(fd, BLKSSZGET, &mut bs) };
        bs as u32
    } else {
        stat.st_blksize as u32
    }
}

/// Returns the device model string for a block device fd, or a
/// fallback description for regular files / unknown devices.
pub fn fd_to_dev_model(fd: RawFd) -> String {
    let stat = fstat(fd);

    if !is_blk(stat.st_mode) {
        return "(image file)".to_string();
    }

    let major = libc::major(stat.st_rdev);
    let minor = libc::minor(stat.st_rdev);
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

/// Returns true if the block device is non-rotational (SSD).
///
/// For regular files, returns false.
pub fn nonrot(fd: RawFd) -> bool {
    let stat = fstat(fd);

    if !is_blk(stat.st_mode) {
        return false;
    }

    let major = libc::major(stat.st_rdev);
    let minor = libc::minor(stat.st_rdev);
    let path = format!("/sys/dev/block/{}:{}/queue/rotational", major, minor);

    match std::fs::read_to_string(&path) {
        Ok(s) => s.trim() == "0",
        Err(_) => false,
    }
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
/// Returns the raw fd on success, or a negative errno on failure.
pub fn open_device(path: &std::ffi::CStr, mode: u32) -> Result<RawFd, i32> {
    let mut flags = 0i32;

    let rw = mode & (BLK_OPEN_READ | BLK_OPEN_WRITE);
    if rw == (BLK_OPEN_READ | BLK_OPEN_WRITE) {
        flags = libc::O_RDWR;
    } else if mode & BLK_OPEN_READ != 0 {
        flags = libc::O_RDONLY;
    } else if mode & BLK_OPEN_WRITE != 0 {
        flags = libc::O_WRONLY;
    }

    if mode & BLK_OPEN_BUFFERED == 0 {
        flags |= libc::O_DIRECT;
    }

    if mode & BLK_OPEN_EXCL != 0 {
        flags |= libc::O_EXCL;
    }

    if mode & BLK_OPEN_CREAT != 0 {
        flags |= libc::O_CREAT;
    }

    let fd = unsafe { libc::open(path.as_ptr(), flags, 0o600) };
    if fd < 0 {
        Err(unsafe { *libc::__errno_location() })
    } else {
        Ok(fd)
    }
}

/// Call the C blkid_check function to probe for existing filesystems.
pub fn blkid_check(fd: RawFd, path: &std::ffi::CStr, force: bool) {
    extern "C" {
        fn blkid_check(fd: libc::c_int, path: *const libc::c_char, force: bool);
    }
    unsafe { blkid_check(fd, path.as_ptr(), force) }
}

fn fstat(fd: RawFd) -> libc::stat {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::fstat(fd, &mut stat) };
    if ret != 0 {
        crate::wrappers::super_io::die("stat error");
    }
    stat
}

fn is_blk(mode: libc::mode_t) -> bool {
    (mode & libc::S_IFMT) == libc::S_IFBLK
}
