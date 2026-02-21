// SPDX-License-Identifier: GPL-2.0

//! Helpers for detecting device-mapper multipath relationships.
//!
//! The core entrypoint is [`find_multipath_holder()`], which walks sysfs
//! holders to determine whether a block device sits under a dm-multipath map.
//! Both top-level maps (`mpath-...`) and partition maps
//! (`part<N>-mpath-...`, including nested partition prefixes) are treated as
//! multipath.
//! This is used by command paths that need to warn or gate operations on
//! multipath component devices.

use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use log::{debug, warn};

const MAX_MULTIPATH_DEPTH: u32 = 8;

fn sysfs_path_for_dev(dev: u64) -> PathBuf {
    let major = libc::major(dev as libc::dev_t);
    let minor = libc::minor(dev as libc::dev_t);
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

fn is_multipath_dm_uuid(uuid: &str) -> bool {
    let mut rest = uuid;

    loop {
        if rest.starts_with("mpath-") {
            return true;
        }

        let Some(next) = rest.strip_prefix("part") else {
            return false;
        };

        let digits = next.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            return false;
        }

        let suffix = &next[digits..];
        let Some(after_dash) = suffix.strip_prefix('-') else {
            return false;
        };

        rest = after_dash;
    }
}

/// Returns the topmost multipath holder for a device, if any.
pub fn find_multipath_holder(path: &Path) -> Option<PathBuf> {
    find_multipath_holder_inner(path, 0)
}

pub fn warn_multipath_component(path: &Path, mpath_dev: &Path) {
    eprintln!(
        "Warning: {} appears to be a multipath component device.",
        path.display()
    );
    eprintln!(
        "Consider using the multipath device ({}) instead.",
        mpath_dev.display()
    );
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

    if !metadata.file_type().is_block_device() {
        debug!("Skipping non-block device {}", path.display());
        return None;
    }

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

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                debug!("Failed to iterate {}: {}", holders.display(), e);
                continue;
            }
        };

        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("dm-") {
            continue;
        }

        let holder_sysfs = PathBuf::from(format!("/sys/block/{}", name));
        let dm_path = holder_sysfs.join("dm");

        // Check if this is a multipath dm UUID (map or map partition)
        let uuid = read_sysfs_attr(&dm_path, "uuid").unwrap_or_default();
        if !is_multipath_dm_uuid(&uuid) {
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
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestTempDir {
        path: PathBuf,
    }

    impl TestTempDir {
        fn new(name: &str) -> Self {
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "bcachefs-device-multipath-{}-{}-{}",
                std::process::id(),
                name,
                ts
            ));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestTempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn read_sysfs_attr_trims_newline() {
        let dir = TestTempDir::new("read-attr");
        let attr = dir.path().join("uuid");
        fs::write(&attr, "mpath-foo\n").unwrap();

        let v = read_sysfs_attr(dir.path(), "uuid");
        assert_eq!(v.as_deref(), Some("mpath-foo"));
    }

    #[test]
    fn read_sysfs_attr_missing_returns_none() {
        let dir = TestTempDir::new("missing-attr");
        let v = read_sysfs_attr(dir.path(), "does-not-exist");
        assert!(v.is_none());
    }

    #[test]
    fn find_multipath_holder_ignores_non_block_files() {
        let dir = TestTempDir::new("non-block");
        let file = dir.path().join("regular-file");
        fs::write(&file, "x").unwrap();

        let holder = find_multipath_holder(&file);
        assert!(holder.is_none());
    }

    #[test]
    fn find_multipath_holder_missing_path_returns_none() {
        let path = PathBuf::from("/definitely/not/present/bcachefs-device-multipath-test");
        assert!(find_multipath_holder(&path).is_none());
    }

    #[test]
    fn detects_map_uuid_forms() {
        assert!(is_multipath_dm_uuid("mpath-3600508b400105e210000900000490000"));
        assert!(is_multipath_dm_uuid("part1-mpath-3600508b400105e210000900000490000"));
        assert!(is_multipath_dm_uuid("part12-mpath-foo"));
        assert!(is_multipath_dm_uuid("part1-part2-mpath-foo"));
        assert!(is_multipath_dm_uuid("part10-part3-part7-mpath-foo"));
    }

    #[test]
    fn rejects_non_multipath_uuid_forms() {
        assert!(!is_multipath_dm_uuid(""));
        assert!(!is_multipath_dm_uuid("LVM-abc"));
        assert!(!is_multipath_dm_uuid("part-mpath-foo"));
        assert!(!is_multipath_dm_uuid("part1mpath-foo"));
        assert!(!is_multipath_dm_uuid("part1-"));
        assert!(!is_multipath_dm_uuid("part1-crypt-foo"));
        assert!(!is_multipath_dm_uuid("foo-mpath-bar"));
    }
}
