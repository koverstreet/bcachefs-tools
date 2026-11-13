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

use bcachefs_kernel::util::ioctl::{_IO, _IOR};

const BLKGETSIZE64: libc::Ioctl = _IOR::<usize>(0x12, 114) as libc::Ioctl;
const BLKPBSZGET: libc::Ioctl = _IO(0x12, 123) as libc::Ioctl;

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
    // Kernel returns !bdev_nonrot(bdev) via put_ushort — bdev_nonrot internally
    // uses bdev_get_queue(bdev), which resolves to the parent disk's queue for
    // partitions and handles LVM, md, loop devices, etc. uniformly.
    const BLKROTATIONAL: libc::Ioctl = _IO(0x12, 126) as libc::Ioctl;
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

mod blkid {
    use libc::{c_char, c_int, c_void};

    pub type Probe = *mut c_void;

    pub const PARTS_MAGIC: c_int   = 1 << 3;
    pub const SUBLKS_LABEL: c_int  = 1 << 1;
    pub const SUBLKS_TYPE: c_int   = 1 << 5;
    pub const SUBLKS_MAGIC: c_int  = 1 << 9;

    #[link(name = "blkid")]
    unsafe extern "C" {
        pub fn blkid_get_library_version(ver_string: *mut *const c_char, date_string: *mut *const c_char) -> c_int;
        pub fn blkid_new_probe() -> Probe;
        pub fn blkid_free_probe(pr: Probe);
        pub fn blkid_probe_set_device(pr: Probe, fd: c_int, off: i64, size: i64) -> c_int;
        pub fn blkid_probe_enable_partitions(pr: Probe, enable: c_int) -> c_int;
        pub fn blkid_probe_set_partitions_flags(pr: Probe, flags: c_int) -> c_int;
        pub fn blkid_probe_enable_superblocks(pr: Probe, enable: c_int) -> c_int;
        pub fn blkid_probe_set_superblocks_flags(pr: Probe, flags: c_int) -> c_int;
        pub fn blkid_do_fullprobe(pr: Probe) -> c_int;
        pub fn blkid_do_probe(pr: Probe) -> c_int;
        pub fn blkid_do_wipe(pr: Probe, dryrun: c_int) -> c_int;
        pub fn blkid_probe_lookup_value(pr: Probe, name: *const c_char, data: *mut *const c_char,
                                        len: *mut usize) -> c_int;
    }

    /// @name's value, as the probe found it.
    pub fn lookup(pr: Probe, name: &std::ffi::CStr) -> Option<String> {
        let mut data = core::ptr::null();
        let mut len = 0;
        unsafe { blkid_probe_lookup_value(pr, name.as_ptr(), &mut data, &mut len) };
        (!data.is_null()).then(|| unsafe { std::ffi::CStr::from_ptr(data) }.to_string_lossy().into_owned())
    }
}

/// Check for existing filesystems using blkid and optionally wipe them.
pub fn blkid_check(fd: RawFd, path: &std::ffi::CStr, force: bool) {
    use crate::wrappers::super_io::die;

    let mut version = core::ptr::null();
    let version_code = unsafe { blkid::blkid_get_library_version(&mut version, core::ptr::null_mut()) };
    let version = unsafe { std::ffi::CStr::from_ptr(version) }.to_string_lossy();
    if version_code < 2401 {
        if force {
            eprintln!("Continuing with out of date libblkid {version} because --force was passed.");
        } else {
            // Reference for picking 2.40.1:
            // https://mirrors.edge.kernel.org/pub/linux/utils/util-linux/v2.40/v2.40.1-ReleaseNotes
            // https://github.com/util-linux/util-linux/issues/3103
            die(&format!("Refusing to format when using libblkid {version}\n\
                          libblkid >= 2.40.1 is required to check for existing filesystems\n\
                          Earlier versions may not recognize some bcachefs filesystems."));
        }
    }

    let pr = unsafe { blkid::blkid_new_probe() };
    if pr.is_null() {
        die("blkid error 1");
    }
    if unsafe { blkid::blkid_probe_set_device(pr, fd, 0, 0) } != 0 {
        die("blkid error 2");
    }
    if unsafe {
        blkid::blkid_probe_enable_partitions(pr, 1) != 0 ||
        blkid::blkid_probe_set_partitions_flags(pr, blkid::PARTS_MAGIC) != 0 ||
        blkid::blkid_probe_enable_superblocks(pr, 1) != 0 ||
        blkid::blkid_probe_set_superblocks_flags(pr,
            blkid::SUBLKS_LABEL | blkid::SUBLKS_TYPE | blkid::SUBLKS_MAGIC) != 0
    } {
        die("blkid error 3");
    }
    if unsafe { blkid::blkid_do_fullprobe(pr) } < 0 {
        die("blkid error 4");
    }

    let fs_type = blkid::lookup(pr, c"TYPE");
    let fs_label = blkid::lookup(pr, c"LABEL");
    let pt_type = blkid::lookup(pr, c"PTTYPE");

    if fs_type.is_some() || pt_type.is_some() {
        let path = path.to_string_lossy();
        if let Some(fs_type) = &fs_type {
            match &fs_label {
                Some(label) => println!("{path} contains a {fs_type} filesystem labelled '{label}'"),
                None        => println!("{path} contains a {fs_type} filesystem"),
            }
        }
        if let Some(pt_type) = &pt_type {
            println!("{path} contains a {pt_type} partition table");
        }

        if !force {
            print!("Proceed anyway (existing signatures will be wiped)?");
            if !crate::util::ask_yn() {
                std::process::exit(1);
            }
        }
        /*
         * Wipe fs superblock and partition-table signatures both (the
         * probe enables BLKID_SUBLKS_MAGIC and BLKID_PARTS_MAGIC), so
         * former-partition fs signatures can't linger to confuse
         * UUID-based device discovery after a whole-disk format.
         */
        while unsafe { blkid::blkid_do_probe(pr) } == 0 {
            if unsafe { blkid::blkid_do_wipe(pr, 0) } != 0 {
                die("Failed to wipe preexisting metadata.");
            }
        }
    }

    unsafe { blkid::blkid_free_probe(pr) };
}

fn fd_metadata(fd: RawFd) -> Metadata {
    match std::fs::metadata(format!("/proc/self/fd/{}", fd)) {
        Ok(metadata) => metadata,
        Err(_) => {
            crate::wrappers::super_io::die("stat error");
        }
    }
}
