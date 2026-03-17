// SPDX-License-Identifier: GPL-2.0

//! Rust implementation of bch2_format and bch2_format_for_device_add.

use std::ffi::{CStr, CString};
use std::os::unix::fs::FileExt;
use std::os::unix::io::RawFd;

use bch_bindgen::c;
use bch_bindgen::{opt_defined, opt_get, opt_set};

use crate::wrappers::super_io::{die, BCHFS_MAGIC, SUPERBLOCK_SIZE_DEFAULT};

/// Device options for formatting — Rust replacement for C struct dev_opts.
///
/// Owns the file descriptor and string data. The fd is closed on drop.
pub struct DevOpts {
    pub fd: RawFd,
    pub path: CString,
    pub label: Option<CString>,
    pub sb_offset: u64,
    pub sb_end: u64,
    pub nbuckets: u64,
    pub fs_size: u64,
    pub opts: c::bch_opts,
}

impl DevOpts {
    /// Create a new DevOpts with the given path, no fd yet.
    pub fn new(path: CString) -> Self {
        DevOpts {
            fd: -1,
            path,
            label: None,
            sb_offset: 0,
            sb_end: 0,
            nbuckets: 0,
            fs_size: 0,
            opts: Default::default(),
        }
    }

    /// Open the device for formatting, with blkid checks.
    ///
    /// Adds READ|WRITE|EXCL|BUFFERED to the given extra flags
    /// (matching the old C open_for_format behavior).
    pub fn open(&mut self, extra_mode: u32, force: bool) -> Result<(), i32> {
        use crate::wrappers::bdev::*;
        let mode = BLK_OPEN_READ | BLK_OPEN_WRITE | BLK_OPEN_EXCL | BLK_OPEN_BUFFERED | extra_mode;
        self.fd = open_device(&self.path, mode)?;
        blkid_check(self.fd, &self.path, force);
        Ok(())
    }

    /// Open the device without blkid checks or default flags (for migrate).
    pub fn open_no_blkid(&mut self, mode: u32) -> Result<(), i32> {
        self.fd = crate::wrappers::bdev::open_device(&self.path, mode)?;
        Ok(())
    }
}

impl Drop for DevOpts {
    fn drop(&mut self) {
        if self.fd >= 0 {
            unsafe { libc::close(self.fd) };
        }
    }
}

/// Features enabled on all new filesystems.
/// Must match BCH_SB_FEATURES_ALL in bcachefs_format.h.
const BCH_SB_FEATURES_ALL: u64 = {
    use c::bch_sb_feature::*;
    // BCH_SB_FEATURES_ALWAYS:
    (1 << BCH_FEATURE_new_extent_overwrite as u64) |
    (1 << BCH_FEATURE_extents_above_btree_updates as u64) |
    (1 << BCH_FEATURE_btree_updates_journalled as u64) |
    (1 << BCH_FEATURE_alloc_v2 as u64) |
    (1 << BCH_FEATURE_extents_across_btree_nodes as u64) |
    // Plus FEATURES_ALL additions:
    (1 << BCH_FEATURE_new_siphash as u64) |
    (1 << BCH_FEATURE_btree_ptr_v2 as u64) |
    (1 << BCH_FEATURE_new_varint as u64) |
    (1 << BCH_FEATURE_journal_no_flush as u64) |
    (1 << BCH_FEATURE_incompat_version_field as u64)
};

const TARGET_DEV_START: u32 = 1;
const TARGET_GROUP_START: u32 = 256 + TARGET_DEV_START;

fn dev_to_target(dev: usize) -> u32 {
    TARGET_DEV_START + dev as u32
}

fn group_to_target(group: u32) -> u32 {
    TARGET_GROUP_START + group
}

/// Resolve a target string (device path or disk group name) to a target id.
fn parse_target(
    sb: &mut c::bch_sb_handle,
    devs: &[DevOpts],
    s: *const std::os::raw::c_char,
) -> u32 {
    if s.is_null() {
        return 0;
    }

    let target_str = unsafe { CStr::from_ptr(s) };
    let target_str_slice = target_str.to_bytes();

    for (idx, dev) in devs.iter().enumerate() {
        if target_str_slice == dev.path.as_bytes() {
            return dev_to_target(idx);
        }
    }

    let idx = unsafe { c::bch2_disk_path_find(sb, s) };
    if idx >= 0 {
        return group_to_target(idx as u32);
    }

    die(&format!("Invalid target {}", target_str.to_string_lossy()));
}

/// Set all sb options from a bch_opts struct.
fn opt_set_sb_all(sb: &mut c::bch_sb, dev_idx: i32, opts: &mut c::bch_opts) {
    use bch_bindgen::opts;

    for (id, opt) in opts::opt_table().iter().enumerate() {
        let opt_id = opts::opt_id(id);

        let v = if opts::opt_defined_by_id(opts, opt_id) {
            opts::opt_get_by_id(opts, opt_id)
        } else {
            opts::opt_get_by_id(opts::opts_default(), opt_id)
        };

        opts::opt_set_sb(sb, dev_idx, opt, v);
    }
}

/// Format one or more devices as a bcachefs filesystem.
///
/// Returns a pointer to the superblock (caller must free with `free()`).
///
/// Exits on fatal errors (matching the C `die()` behavior).
pub fn format(
    fs_opt_strs: c::bch_opt_strs,
    mut fs_opts: c::bch_opts,
    mut opts: c::format_opts,
    dev_slice: &mut [DevOpts],
) -> *mut c::bch_sb {

    // Get device size if not specified (needed for block size threshold)
    for dev in dev_slice.iter_mut() {
        if dev.fs_size == 0 {
            dev.fs_size = crate::wrappers::bdev::get_size(dev.fd);
        }
    }

    if opt_defined!(fs_opts, block_size) == 0 {
        let block_size = pick_block_size(&fs_opts, dev_slice);
        opt_set!(fs_opts, block_size, block_size as u16);
    }

    if fs_opts.block_size < 512 {
        die(&format!(
            "blocksize too small: {}, must be greater than one sector (512 bytes)",
            fs_opts.block_size
        ));
    }

    // Calculate bucket sizes
    let fs_bucket_size = pick_bucket_size(&fs_opts, dev_slice);

    for dev in dev_slice.iter_mut() {
        let opts = &mut dev.opts;
        if opt_defined!(opts, bucket_size) == 0 {
            let clamped = dev_bucket_size_clamp(fs_opts, dev.fs_size, fs_bucket_size);
            opt_set!(opts, bucket_size, clamped as u32);
        }
    }

    for dev in dev_slice.iter_mut() {
        dev.nbuckets = dev.fs_size / dev.opts.bucket_size as u64;
        check_bucket_size(&fs_opts, dev);
    }

    // Calculate btree node size
    if opt_defined!(fs_opts, btree_node_size) == 0 {
        let mut s = bch_bindgen::opts::opts_default().btree_node_size;
        for dev in dev_slice.iter() {
            s = s.min(dev.opts.bucket_size);
        }
        opt_set!(fs_opts, btree_node_size, s);
    }

    if fs_opts.btree_node_size <= fs_opts.block_size as u32 {
        die(&format!(
            "btree node size ({}) must be greater than block size ({})",
            fs_opts.btree_node_size, fs_opts.block_size
        ));
    }

    // UUID
    if opts.uuid.b == [0u8; 16] {
        opts.uuid.b = *uuid::Uuid::new_v4().as_bytes();
    }

    // Allocate superblock
    // ManuallyDrop: we return sb.sb to the caller (who frees it),
    // so we must not let bch_sb_handle's Drop call bch2_free_super.
    let mut sb = std::mem::ManuallyDrop::new(c::bch_sb_handle::default());
    if unsafe { c::bch2_sb_realloc(&mut *sb, 0) } != 0 {
        die("insufficient memory");
    }

    sb.sb_mut().version = (opts.version as u16).to_le();
    sb.sb_mut().version_min = (opts.version as u16).to_le();
    sb.sb_mut().magic.b = BCHFS_MAGIC;
    sb.sb_mut().user_uuid = opts.uuid;
    sb.sb_mut().nr_devices = dev_slice.len() as u8;

    sb.sb_mut().set_sb_version_incompat_allowed(opts.version as u64);
    // These are no longer options, only for compatibility with old versions
    sb.sb_mut().set_sb_meta_replicas_req(1);
    sb.sb_mut().set_sb_data_replicas_req(1);
    sb.sb_mut().set_sb_extent_bp_shift(16);

    let version_threshold =
        c::bcachefs_metadata_version::bcachefs_metadata_version_disk_accounting_big_endian as u32;
    if opts.version > version_threshold {
        sb.sb_mut().features[0] |= BCH_SB_FEATURES_ALL.to_le();
    }

    // Internal UUID (different from user_uuid)
    sb.sb_mut().uuid.b = *uuid::Uuid::new_v4().as_bytes();

    // Label
    if !opts.label.is_null() {
        let label = unsafe { CStr::from_ptr(opts.label) };
        let label_bytes = label.to_bytes();
        if label_bytes.len() >= sb.sb().label.len() {
            die(&format!(
                "filesystem label too long (max {} characters)",
                sb.sb().label.len() - 1
            ));
        }
        sb.sb_mut().label[..label_bytes.len()].copy_from_slice(label_bytes);
    }

    // Create ext field before setting options - some options (e.g.
    // dev_readahead) use set_ext which requires this field to exist
    let ext_u64s = (std::mem::size_of::<c::bch_sb_field_ext>() / std::mem::size_of::<u64>()) as u32;
    sb.field_get_minsize::<c::bch_sb_field_ext>(ext_u64s);

    opt_set_sb_all(sb.sb_mut(), -1, &mut fs_opts);

    // Time
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_else(|_| die("error getting current time"));
    let nsec = now.as_secs() * 1_000_000_000 + now.subsec_nanos() as u64;
    sb.sb_mut().time_base_lo = nsec.to_le();
    sb.sb_mut().time_precision = 1u32.to_le();

    // Member info
    let mi_size = std::mem::size_of::<c::bch_sb_field_members_v2>()
        + std::mem::size_of::<c::bch_member>() * dev_slice.len();
    let mi_u64s = mi_size / std::mem::size_of::<u64>();

    let mi = bch_bindgen::sb::sb_field_resize::<c::bch_sb_field_members_v2>(&mut *sb, mi_u64s as u32)
        .unwrap_or_else(|| die("failed to resize members_v2 field"));
    mi.member_bytes = (std::mem::size_of::<c::bch_member>() as u16).to_le();

    for (idx, dev) in dev_slice.iter_mut().enumerate() {
        let m = sb.member_mut(idx as u32)
            .unwrap_or_else(|| die("member index out of range"));
        m.uuid.b = *uuid::Uuid::new_v4().as_bytes();
        m.nbuckets = dev.nbuckets.to_le();
        m.first_bucket = 0;

        let fd = dev.fd;
        let opts = &mut dev.opts;
        if opt_defined!(opts, rotational) == 0 {
            let nonrot = crate::wrappers::bdev::nonrot(fd);
            opt_set!(opts, rotational, !nonrot as u8);
        }

        opt_set_sb_all(sb.sb_mut(), idx as i32, &mut dev.opts);
        sb.member_mut(idx as u32).unwrap().set_member_rotational_set(1);
    }

    // Disk labels
    for (idx, dev) in dev_slice.iter().enumerate() {
        let label = match dev.label.as_deref() {
            Some(l) => l,
            None => continue,
        };

        let path_idx = unsafe { c::bch2_disk_path_find_or_create(&mut *sb, label.as_ptr()) };
        if path_idx < 0 {
            die(&format!(
                "error creating disk path: {}",
                std::io::Error::from_raw_os_error(-path_idx)
            ));
        }

        // Recompute m after sb modification (memory may have been reallocated)
        sb.member_mut(idx as u32).unwrap().set_member_group(path_idx as u64 + 1);
    }

    // Targets
    let target_strs = unsafe { &fs_opt_strs.__bindgen_anon_1.__bindgen_anon_1 };
    let sb_handle = &mut *sb;
    let foreground = parse_target(sb_handle, dev_slice, target_strs.foreground_target);
    let background = parse_target(sb_handle, dev_slice, target_strs.background_target);
    let promote    = parse_target(sb_handle, dev_slice, target_strs.promote_target);
    let metadata   = parse_target(sb_handle, dev_slice, target_strs.metadata_target);
    sb.sb_mut().set_sb_foreground_target(foreground as u64);
    sb.sb_mut().set_sb_background_target(background as u64);
    sb.sb_mut().set_sb_promote_target(promote as u64);
    sb.sb_mut().set_sb_metadata_target(metadata as u64);

    // Encryption
    if opts.encrypted {
        let crypt_u64s =
            std::mem::size_of::<c::bch_sb_field_crypt>() / std::mem::size_of::<u64>();
        let crypt: &mut c::bch_sb_field_crypt = sb.field_resize(crypt_u64s as u32)
            .unwrap_or_else(|| die("failed to create crypt field"));
        let crypt_ptr = crypt as *mut c::bch_sb_field_crypt;
        unsafe { c::bch_sb_crypt_init(sb.sb, crypt_ptr, opts.passphrase) };
        sb.sb_mut().set_sb_encryption_type(1);
    }

    unsafe { c::bch2_sb_members_cpy_v2_v1(&mut *sb) };

    // Write superblocks to each device
    for (idx, dev) in dev_slice.iter_mut().enumerate() {
        let size_sectors = dev.fs_size >> 9;
        sb.sb_mut().dev_idx = idx as u8;

        if dev.sb_offset == 0 {
            dev.sb_offset = c::BCH_SB_SECTOR as u64;
            dev.sb_end = size_sectors;
        }

        if let Err(e) = crate::wrappers::super_io::sb_layout_init(
            &mut sb.sb_mut().layout,
            fs_opts.block_size as u32,
            dev.opts.bucket_size,
            opts.superblock_size,
            dev.sb_offset,
            dev.sb_end,
            opts.no_sb_at_end,
        ) {
            eprintln!("Error: {}", e);
            return std::ptr::null_mut();
        }

        let fd = dev.fd;

        if dev.sb_offset == c::BCH_SB_SECTOR as u64 {
            // Zero start of disk
            let zeroes = vec![0u8; (c::BCH_SB_SECTOR as usize) << 9];
            let file = crate::wrappers::super_io::borrowed_file(fd);
            file.write_all_at(&zeroes, 0)
                .unwrap_or_else(|e| die(&format!("zeroing start of disk: {}", e)));
        }

        crate::wrappers::super_io::bch2_super_write(fd, sb.sb);
    }

    // udevadm trigger --settle <devices>
    let mut udevadm = std::process::Command::new("udevadm");
    udevadm.args(["trigger", "--settle"]);
    for dev in dev_slice.iter() {
        udevadm.arg(dev.path.to_str().unwrap_or(""));
    }
    let _ = udevadm.status();

    sb.sb
}

/// Format a single device for addition to an existing filesystem.
pub fn format_for_device_add(
    dev: &mut DevOpts,
    block_size: u32,
    btree_node_size: u32,
) -> i32 {
    let fs_opt_strs: c::bch_opt_strs = Default::default();
    let mut fs_opts = fs_opt_strs.parse();
    opt_set!(fs_opts, block_size, block_size as u16);
    opt_set!(fs_opts, btree_node_size, btree_node_size);

    let fmt_opts = format_opts_default();
    let sb = format(fs_opt_strs, fs_opts, fmt_opts, std::slice::from_mut(dev));
    unsafe { libc::free(sb as *mut _) };

    0
}

/// Mirrors the C `format_opts_default()` inline function.
pub(crate) fn format_opts_default() -> c::format_opts {
    // Try to load bcachefs module to detect kernel version
    let _ = std::process::Command::new("modprobe")
        .arg("bcachefs")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    let kernel_version = crate::wrappers::sysfs::bcachefs_kernel_version() as u32;
    let current =
        c::bcachefs_metadata_version::bcachefs_metadata_version_max as u32 - 1;

    let version = if kernel_version > 0 {
        current.min(kernel_version)
    } else {
        current
    };

    c::format_opts {
        version,
        superblock_size: SUPERBLOCK_SIZE_DEFAULT,
        ..Default::default()
    }
}

/// Clamp fs-wide bucket size for a specific device that may be too small.
///
/// Prefer at least 2048 buckets per device (512 is the absolute minimum
/// but gets dicey). Within that constraint, try to reach at least
/// encoded_extent_max to avoid fragmenting checksummed/compressed extents.
fn dev_bucket_size_clamp(fs_opts: c::bch_opts, dev_size: u64, fs_bucket_size: u64) -> u64 {
    let min_nr_nbuckets = c::BCH_MIN_NR_NBUCKETS as u64;

    // Largest bucket size that still gives >= 2048 buckets
    let mut max_size = rounddown_pow_of_two(dev_size / (min_nr_nbuckets * 4));
    if opt_defined!(fs_opts, btree_node_size) != 0 {
        max_size = max_size.max(fs_opts.btree_node_size as u64);
    }
    if max_size * min_nr_nbuckets > dev_size {
        die(&format!("bucket size {} too big for device size", max_size));
    }

    let mut dev_bucket_size = max_size.min(fs_bucket_size);

    // Buckets >= encoded_extent_max avoid fragmenting encoded extents
    let extent_min = opt_get!(fs_opts, encoded_extent_max) as u64;
    while dev_bucket_size < extent_min && dev_bucket_size < max_size {
        dev_bucket_size *= 2;
    }

    dev_bucket_size
}

fn total_system_ram() -> u64 {
    let mut info: libc::sysinfo = unsafe { std::mem::zeroed() };
    if unsafe { libc::sysinfo(&mut info) } != 0 {
        die("sysinfo() failed");
    }
    info.totalram as u64 * info.mem_unit as u64
}

fn rounddown_pow_of_two(v: u64) -> u64 {
    if v == 0 {
        return 0;
    }
    1u64 << (63 - v.leading_zeros())
}

/// Pick the filesystem-wide block size based on device sizes and topology.
///
/// On large filesystems (>= 1GB), use the maximum of 4k and the device
/// physical block size for performance on 4k-sector (or larger) hardware.
/// On small filesystems (typically test images on loop devices), default
/// to 512 bytes to avoid wasting space.
///
/// Returns the block size in bytes.
pub fn pick_block_size(_fs_opts: &c::bch_opts, dev_slice: &[DevOpts]) -> u32 {
    let total_size: u64 = dev_slice.iter().map(|d| d.fs_size).sum();

    let block_size = if total_size >= 1u64 << 30 {
        let mut bs = 4096u32;
        for dev in dev_slice.iter() {
            bs = bs.max(crate::wrappers::bdev::get_blocksize(dev.fd));
        }
        bs
    } else {
        512u32
    };

    block_size
        .min((u16::MAX as u32 >> 1) + 1)
}

/// Pick the filesystem-wide bucket size based on device sizes and options.
///
/// Returns the bucket size in bytes.
pub fn pick_bucket_size(opts: &c::bch_opts, devs: &[DevOpts]) -> u64 {
    // Hard minimum: bucket must hold a btree node
    let mut bucket_size = opts.block_size as u64;
    if opt_defined!(opts, btree_node_size) != 0 {
        bucket_size = bucket_size.max(opts.btree_node_size as u64);
    }

    let min_dev_size = c::BCH_MIN_NR_NBUCKETS as u64 * bucket_size;
    for dev in devs {
        if dev.fs_size < min_dev_size {
            let path = dev.path.to_string_lossy();
            die(&format!(
                "cannot format {}, too small ({} bytes, min {})",
                path, dev.fs_size, min_dev_size
            ));
        }
    }

    let total_fs_size: u64 = devs.iter().map(|d| d.fs_size).sum();

    // Soft preferences below — these set the ideal bucket size,
    // but dev_bucket_size_clamp() may reduce per-device to keep
    // bucket counts reasonable on small devices

    // btree_node_size isn't calculated yet; use a reasonable floor
    bucket_size = bucket_size.max(256 << 10);

    // Avoid fragmenting encoded (checksummed/compressed) extents
    // when they're moved — prefer buckets large enough for several
    // max-size extents
    bucket_size = bucket_size.max(opt_get!(opts, encoded_extent_max) as u64 * 4);

    // Prefer larger buckets up to 2MB — reduces allocator overhead.
    // Scales linearly with total filesystem size, reaching 2MB at 2TB
    let perf_lower_bound = (2u64 << 20).min(total_fs_size / (1u64 << 20));
    bucket_size = bucket_size.max(perf_lower_bound);

    // Upper bound on bucket count: ensure we can fsck with available
    // memory. Large fudge factor to allow for other fsck processes
    // and devices being added after creation
    let total_ram = total_system_ram();
    let mem_available_for_fsck = total_ram / 8;
    let bucket_struct_size = std::mem::size_of::<c::bucket>() as u64;
    let buckets_can_fsck = mem_available_for_fsck / (bucket_struct_size * 3 / 2);
    let mem_lower_bound = (total_fs_size / buckets_can_fsck).next_power_of_two();
    bucket_size = bucket_size.max(mem_lower_bound);

    bucket_size
        .next_power_of_two()
        .min((u32::MAX as u64 >> 1) + 1)
}

/// Validate that a device's bucket size is consistent with filesystem options.
///
/// Exits on validation failure (matches C `die()` behavior).
pub fn check_bucket_size(opts: &c::bch_opts, dev: &DevOpts) {
    if dev.opts.bucket_size < opts.block_size as u32 {
        die(&format!(
            "Bucket size ({}) cannot be smaller than block size ({})",
            dev.opts.bucket_size, opts.block_size
        ));
    }

    if opt_defined!(opts, btree_node_size) != 0
        && dev.opts.bucket_size < opts.btree_node_size
    {
        die(&format!(
            "Bucket size ({}) cannot be smaller than btree node size ({})",
            dev.opts.bucket_size, opts.btree_node_size
        ));
    }

    if dev.nbuckets < c::BCH_MIN_NR_NBUCKETS as u64 {
        die(&format!(
            "Not enough buckets: {}, need {} (bucket size {})",
            dev.nbuckets,
            c::BCH_MIN_NR_NBUCKETS,
            dev.opts.bucket_size
        ));
    }
}

