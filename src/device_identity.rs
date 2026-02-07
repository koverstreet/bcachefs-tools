// SPDX-License-Identifier: GPL-2.0

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use log::{debug, warn};

const MAX_MULTIPATH_DEPTH: u32 = 8;

fn sysfs_path_for_dev(dev: u64) -> PathBuf {
    let major = unsafe { libc::major(dev as libc::dev_t) };
    let minor = unsafe { libc::minor(dev as libc::dev_t) };
    PathBuf::from(format!("/sys/dev/block/{}:{}", major, minor))
}

fn read_sysfs_attr(path: &Path, attr: &str) -> Option<String> {
    let full_path = path.join(attr);
    match fs::read_to_string(&full_path) {
        Ok(s) => Some(s.trim().to_string()),
        Err(e) => {
            debug!("Failed to read {}: {}", full_path.display(), e);
            None
        }
    }
}

/// Returns the topmost multipath holder for a device, if any.
pub fn find_multipath_holder(path: &Path) -> Option<PathBuf> {
    find_multipath_holder_inner(path, 0)
}

fn find_multipath_holder_inner(path: &Path, depth: u32) -> Option<PathBuf> {
    if depth >= MAX_MULTIPATH_DEPTH {
        warn!(
            "Reached maximum multipath holder depth ({}) at {}. \
             This may indicate a circular holder relationship or unusually deep device stacking.",
            MAX_MULTIPATH_DEPTH,
            path.display()
        );
        return None;
    }

    let metadata = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            debug!("Failed to stat {}: {}", path.display(), e);
            return None;
        }
    };

    let dev = metadata.rdev();
    if dev == 0 {
        return None;
    }

    let sysfs = sysfs_path_for_dev(dev);
    let holders = sysfs.join("holders");

    let entries = match fs::read_dir(&holders) {
        Ok(e) => e,
        Err(e) => {
            debug!("Failed to read holders dir {}: {}", holders.display(), e);
            return None;
        }
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("dm-") {
            continue;
        }

        let holder_sysfs = PathBuf::from(format!("/sys/block/{}", name));
        let dm_path = holder_sysfs.join("dm");

        // Check if this is a multipath device (DM UUID starting with "mpath-")
        let uuid = read_sysfs_attr(&dm_path, "uuid").unwrap_or_default();
        if !uuid.starts_with("mpath-") {
            continue;
        }

        let mpath_dev = if let Some(dm_name) = read_sysfs_attr(&dm_path, "name") {
            let mapper_path = PathBuf::from(format!("/dev/mapper/{}", dm_name));
            if mapper_path.exists() {
                mapper_path
            } else {
                PathBuf::from(format!("/dev/{}", name))
            }
        } else {
            PathBuf::from(format!("/dev/{}", name))
        };

        if let Some(higher) = find_multipath_holder_inner(&mpath_dev, depth + 1) {
            debug!(
                "Found higher multipath holder: {} -> {}",
                mpath_dev.display(),
                higher.display()
            );
            return Some(higher);
        }

        debug!(
            "Found topmost multipath holder for {}: {}",
            path.display(),
            mpath_dev.display()
        );
        return Some(mpath_dev);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sysfs_path_for_dev() {
        let path = sysfs_path_for_dev(0);
        assert!(path.to_string_lossy().contains("/sys/dev/block/"));
    }
}
