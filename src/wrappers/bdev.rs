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

/// Check for existing filesystems using blkid and optionally wipe them.
pub fn blkid_check(fd: std::os::fd::BorrowedFd<'_>, path: &std::ffi::CStr, force: bool) {
    use crate::util::die;
    use rsblkid::device::TagName;
    use rsblkid::probe::{FsProperty, PartitionScanningOption, Probe, ScanResult};

    let lib = rsblkid::utils::misc::library_version();
    if lib.release_code() < 2401 {
        let version = lib.version_string();
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

    let file = fd.try_clone_to_owned()
        .unwrap_or_else(|e| die(&format!("blkid: duplicating the device's fd: {e}")));
    let mut pr = Probe::builder()
        .scan_file(std::fs::File::from(file))
        .allow_writes()
        .scan_device_partitions(true)
        .partitions_scanning_options([PartitionScanningOption::Magic])
        .collect_fs_properties([FsProperty::Label, FsProperty::Type, FsProperty::Magic])
        .build()
        .unwrap_or_else(|e| die(&format!("blkid: {e}")));

    if matches!(pr.find_device_properties(), ScanResult::Error | ScanResult::ConflictingValues | ScanResult::Exception(_)) {
        die("blkid: probing for existing filesystems failed");
    }

    // what's there by blkid_probe_has_value(): a type rsblkid has no name for
    // is still there
    let (has_fs, has_pt) = (pr.device_property_has_value(TagName::Type), pr.device_property_has_value(TagName::PtType));
    if has_fs || has_pt {
        let path = path.to_string_lossy();
        let value = |pr: &mut Probe, name| pr.lookup_device_property_value(name).map(|t| t.value().to_owned());
        if has_fs {
            let fs_type = value(&mut pr, TagName::Type).unwrap_or_else(|| "unrecognised".into());
            match value(&mut pr, TagName::Label) {
                Some(label) => println!("{path} contains a {fs_type} filesystem labelled '{label}'"),
                None        => println!("{path} contains a {fs_type} filesystem"),
            }
        }
        if has_pt {
            let pt_type = value(&mut pr, TagName::PtType).unwrap_or_else(|| "unrecognised".into());
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
         * probe collects both magics), so former-partition fs signatures
         * can't linger to confuse UUID-based device discovery after a
         * whole-disk format.
         */
        while let ScanResult::FoundProperties = pr.run_scan() {
            if pr.delete_properties_from_device().is_err() {
                die("Failed to wipe preexisting metadata.");
            }
        }
    }
}

fn fd_metadata(fd: RawFd) -> Metadata {
    match std::fs::metadata(format!("/proc/self/fd/{}", fd)) {
        Ok(metadata) => metadata,
        Err(_) => {
            crate::util::die("stat error");
        }
    }
}
