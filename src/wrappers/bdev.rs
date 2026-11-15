// SPDX-License-Identifier: GPL-2.0

//! Block device utilities.
//!
//! get_blocksize(), fd_to_dev_model(), blkid_check(): these work on any fd
//! (block device or regular file).

use std::fs::Metadata;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::io::RawFd;
use std::path::PathBuf;

/// Returns the physical block size of a block device (the _larger_ of the two),
/// with fallback to the filesystem block size hint for regular files, in bytes.
/// (to be used as a performance hint only)
pub fn get_blocksize_physical_hint(fd: impl AsFd) -> u32 {
    let metadata = fd_metadata(fd.as_fd().as_raw_fd());

    if metadata.file_type().is_block_device() {
        rustix::fs::ioctl_blkpbszget(fd).unwrap_or(0)
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

/// Open a block device or file for formatting, as BLK_OPEN_* @mode says.
/// Returns the owned fd on success, or a negative errno on failure.
pub fn open_device(path: &std::ffi::CStr, mode: u32) -> Result<OwnedFd, i32> {
    rustix::fs::open(path, bcachefs_kernel::os::blkdev::open_flags(mode), rustix::fs::Mode::from_raw_mode(0o600))
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
    use crate::util::die;

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
            crate::util::die("stat error");
        }
    }
}
