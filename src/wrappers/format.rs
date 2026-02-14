// SPDX-License-Identifier: GPL-2.0

//! Rust implementation of bch2_format and bch2_format_for_device_add.

use std::ffi::CStr;
use std::os::unix::fs::FileExt;

use bch_bindgen::c;
use bch_bindgen::{opt_defined, opt_get, opt_set};

use super::super_io::{BCHFS_MAGIC, SUPERBLOCK_SIZE_DEFAULT};

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
    devs: &[c::dev_opts],
    s: *const std::os::raw::c_char,
) -> u32 {
    if s.is_null() {
        return 0;
    }

    let target_str = unsafe { CStr::from_ptr(s) };

    for (idx, dev) in devs.iter().enumerate() {
        if !dev.path.is_null() {
            let dev_path = unsafe { CStr::from_ptr(dev.path) };
            if target_str == dev_path {
                return dev_to_target(idx);
            }
        }
    }

    let idx = unsafe { c::bch2_disk_path_find(sb, s) };
    if idx >= 0 {
        return group_to_target(idx as u32);
    }

    panic!("Invalid target {}", target_str.to_string_lossy());
}

/// Set all sb options from a bch_opts struct.
fn opt_set_sb_all(sb: *mut c::bch_sb, dev_idx: i32, opts: &mut c::bch_opts) {
    let nr = c::bch_opt_id::bch2_opts_nr as u32;
    for id in 0..nr {
        let opt_id: c::bch_opt_id = unsafe { std::mem::transmute::<u32, c::bch_opt_id>(id) };

        let v = if unsafe { c::bch2_opt_defined_by_id(opts, opt_id) } {
            unsafe { c::bch2_opt_get_by_id(opts, opt_id) }
        } else {
            unsafe { c::bch2_opt_get_by_id(&c::bch2_opts_default, opt_id) }
        };

        let opt = unsafe { c::bch2_opt_table.as_ptr().add(id as usize) };
        unsafe { c::__bch2_opt_set_sb(sb, dev_idx, opt, v) };
    }
}

/// Format one or more devices as a bcachefs filesystem.
///
/// Returns a pointer to the superblock (caller must free with `free()`).
///
/// Panics on fatal errors (matching the C `die()` behavior).
#[no_mangle]
pub extern "C" fn bch2_format(
    fs_opt_strs: c::bch_opt_strs,
    mut fs_opts: c::bch_opts,
    mut opts: c::format_opts,
    devs: c::dev_opts_list,
) -> *mut c::bch_sb {
    let dev_slice = unsafe { std::slice::from_raw_parts_mut(devs.data, devs.nr) };

    // Calculate block size
    if opt_defined!(fs_opts, block_size) == 0 {
        let mut max_dev_block_size = 4096u32;
        for dev in dev_slice.iter() {
            let bs = unsafe { c::get_blocksize((*dev.bdev).bd_fd) };
            max_dev_block_size = max_dev_block_size.max(bs);
        }
        opt_set!(fs_opts, block_size, max_dev_block_size as u16);
    }

    if fs_opts.block_size < 512 {
        panic!(
            "blocksize too small: {}, must be greater than one sector (512 bytes)",
            fs_opts.block_size
        );
    }

    // Get device size if not specified
    for dev in dev_slice.iter_mut() {
        if dev.fs_size == 0 {
            dev.fs_size = unsafe { c::get_size((*dev.bdev).bd_fd) };
        }
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
        let mut s = unsafe { c::bch2_opts_default.btree_node_size };
        for dev in dev_slice.iter() {
            s = s.min(dev.opts.bucket_size);
        }
        opt_set!(fs_opts, btree_node_size, s);
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
        panic!("insufficient memory");
    }

    let sb_ptr = sb.sb;
    let sb_ref = unsafe { &mut *sb_ptr };

    sb_ref.version = (opts.version as u16).to_le();
    sb_ref.version_min = (opts.version as u16).to_le();
    sb_ref.magic.b = BCHFS_MAGIC;
    sb_ref.user_uuid = opts.uuid;
    sb_ref.nr_devices = devs.nr as u8;

    sb_ref.set_sb_version_incompat_allowed(opts.version as u64);
    // These are no longer options, only for compatibility with old versions
    sb_ref.set_sb_meta_replicas_req(1);
    sb_ref.set_sb_data_replicas_req(1);
    sb_ref.set_sb_extent_bp_shift(16);

    let version_threshold =
        c::bcachefs_metadata_version::bcachefs_metadata_version_disk_accounting_big_endian as u32;
    if opts.version > version_threshold {
        sb_ref.features[0] |= BCH_SB_FEATURES_ALL.to_le();
    }

    // Internal UUID (different from user_uuid)
    sb_ref.uuid.b = *uuid::Uuid::new_v4().as_bytes();

    // Label
    if !opts.label.is_null() {
        let label = unsafe { CStr::from_ptr(opts.label) };
        let label_bytes = label.to_bytes();
        if label_bytes.len() >= sb_ref.label.len() {
            panic!(
                "filesystem label too long (max {} characters)",
                sb_ref.label.len() - 1
            );
        }
        sb_ref.label[..label_bytes.len()].copy_from_slice(label_bytes);
    }

    opt_set_sb_all(sb_ptr, -1, &mut fs_opts);

    // Time
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("error getting current time");
    let nsec = now.as_secs() * 1_000_000_000 + now.subsec_nanos() as u64;
    sb_ref.time_base_lo = nsec.to_le();
    sb_ref.time_precision = 1u32.to_le();

    // Member info
    let mi_size = std::mem::size_of::<c::bch_sb_field_members_v2>()
        + std::mem::size_of::<c::bch_member>() * devs.nr;
    let mi_u64s = mi_size / std::mem::size_of::<u64>();

    let mi = unsafe {
        c::bch2_sb_field_resize_id(
            &mut *sb,
            c::bch_sb_field_type::BCH_SB_FIELD_members_v2,
            mi_u64s as u32,
        ) as *mut c::bch_sb_field_members_v2
    };
    unsafe {
        (*mi).member_bytes = (std::mem::size_of::<c::bch_member>() as u16).to_le();
    }

    for (idx, dev) in dev_slice.iter_mut().enumerate() {
        let m = unsafe { c::bch2_members_v2_get_mut(sb.sb, idx as i32) };

        unsafe {
            (*m).uuid.b = *uuid::Uuid::new_v4().as_bytes();
            (*m).nbuckets = dev.nbuckets.to_le();
            (*m).first_bucket = 0;
        }

        let opts = &mut dev.opts;
        if opt_defined!(opts, rotational) == 0 {
            let nonrot = unsafe { c::bdev_nonrot(dev.bdev) };
            opt_set!(opts, rotational, !nonrot as u8);
        }

        opt_set_sb_all(sb.sb, idx as i32, &mut dev.opts);
        unsafe { &mut *m }.set_member_rotational_set(1);
    }

    // Disk labels
    for (idx, dev) in dev_slice.iter().enumerate() {
        if dev.label.is_null() {
            continue;
        }

        let path_idx = unsafe { c::bch2_disk_path_find_or_create(&mut *sb, dev.label) };
        if path_idx < 0 {
            panic!(
                "error creating disk path: {}",
                std::io::Error::from_raw_os_error(-path_idx)
            );
        }

        // Recompute m after sb modification (memory may have been reallocated)
        let m = unsafe { c::bch2_members_v2_get_mut(sb.sb, idx as i32) };
        unsafe { &mut *m }.set_member_group(path_idx as u64 + 1);
    }

    // Targets
    let target_strs = unsafe { &fs_opt_strs.__bindgen_anon_1.__bindgen_anon_1 };
    let sb_handle = &mut *sb;
    let foreground = parse_target(sb_handle, dev_slice, target_strs.foreground_target);
    let background = parse_target(sb_handle, dev_slice, target_strs.background_target);
    let promote    = parse_target(sb_handle, dev_slice, target_strs.promote_target);
    let metadata   = parse_target(sb_handle, dev_slice, target_strs.metadata_target);
    let sb_ref = unsafe { &mut *sb.sb };
    sb_ref.set_sb_foreground_target(foreground as u64);
    sb_ref.set_sb_background_target(background as u64);
    sb_ref.set_sb_promote_target(promote as u64);
    sb_ref.set_sb_metadata_target(metadata as u64);

    // Encryption
    if opts.encrypted {
        let crypt_size =
            std::mem::size_of::<c::bch_sb_field_crypt>() / std::mem::size_of::<u64>();
        let crypt = unsafe {
            c::bch2_sb_field_resize_id(
                &mut *sb,
                c::bch_sb_field_type::BCH_SB_FIELD_crypt,
                crypt_size as u32,
            ) as *mut c::bch_sb_field_crypt
        };
        unsafe { c::bch_sb_crypt_init(sb.sb, crypt, opts.passphrase) };
        unsafe { &mut *sb.sb }.set_sb_encryption_type(1);
    }

    unsafe { c::bch2_sb_members_cpy_v2_v1(&mut *sb) };

    // Write superblocks to each device
    for (idx, dev) in dev_slice.iter_mut().enumerate() {
        let size_sectors = dev.fs_size >> 9;
        let sb_ref = unsafe { &mut *sb.sb };
        sb_ref.dev_idx = idx as u8;

        if dev.sb_offset == 0 {
            dev.sb_offset = c::BCH_SB_SECTOR as u64;
            dev.sb_end = size_sectors;
        }

        if let Err(e) = super::super_io::sb_layout_init(
            unsafe { &mut (*sb.sb).layout },
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

        let fd = unsafe { (*dev.bdev).bd_fd };

        if dev.sb_offset == c::BCH_SB_SECTOR as u64 {
            // Zero start of disk
            let zeroes = vec![0u8; (c::BCH_SB_SECTOR as usize) << 9];
            let file = super::super_io::borrowed_file(fd);
            file.write_all_at(&zeroes, 0)
                .unwrap_or_else(|e| panic!("zeroing start of disk: {}", e));
        }

        super::super_io::bch2_super_write(fd, sb.sb);

        unsafe { libc::close(fd) };
    }

    // udevadm trigger --settle <devices>
    let mut udevadm = std::process::Command::new("udevadm");
    udevadm.args(["trigger", "--settle"]);
    for dev in dev_slice.iter() {
        if !dev.path.is_null() {
            let path = unsafe { CStr::from_ptr(dev.path) };
            udevadm.arg(path.to_str().unwrap_or(""));
        }
    }
    let _ = udevadm.status();

    sb.sb
}

/// Format a single device for addition to an existing filesystem.
#[no_mangle]
pub extern "C" fn bch2_format_for_device_add(
    dev: *mut c::dev_opts,
    block_size: u32,
    btree_node_size: u32,
) -> i32 {
    let fs_opt_strs: c::bch_opt_strs = Default::default();
    let mut fs_opts = unsafe { c::bch2_parse_opts(fs_opt_strs) };
    opt_set!(fs_opts, block_size, block_size as u16);
    opt_set!(fs_opts, btree_node_size, btree_node_size);

    let devs = c::dev_opts_list {
        nr: 1,
        size: 1,
        data: dev,
        preallocated: Default::default(),
    };

    let fmt_opts = format_opts_default();
    let sb = bch2_format(fs_opt_strs, fs_opts, fmt_opts, devs);
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
        panic!("bucket size {} too big for device size", max_size);
    }

    let mut dev_bucket_size = max_size.min(fs_bucket_size);

    // Buckets >= encoded_extent_max avoid fragmenting encoded extents
    let extent_min = opt_get!(fs_opts, encoded_extent_max) as u64;
    while dev_bucket_size < extent_min && dev_bucket_size < max_size {
        dev_bucket_size *= 2;
    }

    dev_bucket_size
}

fn rounddown_pow_of_two(v: u64) -> u64 {
    if v == 0 {
        return 0;
    }
    1u64 << (63 - v.leading_zeros())
}

/// Pick the filesystem-wide bucket size based on device sizes and options.
///
/// Returns the bucket size in bytes.
pub fn pick_bucket_size(opts: &c::bch_opts, devs: &[c::dev_opts]) -> u64 {
    // Hard minimum: bucket must hold a btree node
    let mut bucket_size = opts.block_size as u64;
    if opt_defined!(opts, btree_node_size) != 0 {
        bucket_size = bucket_size.max(opts.btree_node_size as u64);
    }

    let min_dev_size = c::BCH_MIN_NR_NBUCKETS as u64 * bucket_size;
    for dev in devs {
        if dev.fs_size < min_dev_size {
            let path = if dev.path.is_null() {
                "<unknown>".to_string()
            } else {
                unsafe { CStr::from_ptr(dev.path) }.to_string_lossy().into_owned()
            };
            panic!(
                "cannot format {}, too small ({} bytes, min {})",
                path, dev.fs_size, min_dev_size
            );
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
    let mut info: libc::sysinfo = unsafe { std::mem::zeroed() };
    if unsafe { libc::sysinfo(&mut info) } != 0 {
        panic!("sysinfo() failed");
    }
    let total_ram = info.totalram as u64 * info.mem_unit as u64;
    let mem_available_for_fsck = total_ram / 8;
    let bucket_struct_size = std::mem::size_of::<c::bucket>() as u64;
    let buckets_can_fsck = mem_available_for_fsck / (bucket_struct_size * 3 / 2);
    let mem_lower_bound = (total_fs_size / buckets_can_fsck).next_power_of_two();
    bucket_size = bucket_size.max(mem_lower_bound);

    bucket_size.next_power_of_two()
}

/// Validate that a device's bucket size is consistent with filesystem options.
///
/// Panics on validation failure (matches C `die()` behavior).
pub fn check_bucket_size(opts: &c::bch_opts, dev: &c::dev_opts) {
    if dev.opts.bucket_size < opts.block_size as u32 {
        panic!(
            "Bucket size ({}) cannot be smaller than block size ({})",
            dev.opts.bucket_size, opts.block_size
        );
    }

    if opt_defined!(opts, btree_node_size) != 0
        && dev.opts.bucket_size < opts.btree_node_size
    {
        panic!(
            "Bucket size ({}) cannot be smaller than btree node size ({})",
            dev.opts.bucket_size, opts.btree_node_size
        );
    }

    if dev.nbuckets < c::BCH_MIN_NR_NBUCKETS as u64 {
        panic!(
            "Not enough buckets: {}, need {} (bucket size {})",
            dev.nbuckets,
            c::BCH_MIN_NR_NBUCKETS,
            dev.opts.bucket_size
        );
    }
}

/// C-compatible wrapper for cmd_migrate.c.
#[no_mangle]
pub extern "C" fn bch2_pick_bucket_size(opts: c::bch_opts, devs: c::dev_opts_list) -> u64 {
    let dev_slice = unsafe { std::slice::from_raw_parts(devs.data, devs.nr) };
    pick_bucket_size(&opts, dev_slice)
}

/// C-compatible wrapper for cmd_migrate.c.
#[no_mangle]
pub extern "C" fn bch2_check_bucket_size(opts: c::bch_opts, dev: *mut c::dev_opts) {
    check_bucket_size(&opts, unsafe { &*dev });
}

