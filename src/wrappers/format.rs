// SPDX-License-Identifier: GPL-2.0

//! Rust implementation of bch2_format and bch2_format_for_device_add.

use std::ffi::CStr;

use bch_bindgen::c;
use bch_bindgen::{opt_defined, opt_get, opt_set};

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
        let opt_id: c::bch_opt_id = unsafe { std::mem::transmute(id) };

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
        let mut max_dev_block_size = 0u32;
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
    // Copy devs for the call — bch2_pick_bucket_size takes by value (C semantics)
    let devs_copy = unsafe { std::ptr::read(&devs) };
    let fs_bucket_size = unsafe { c::bch2_pick_bucket_size(fs_opts, devs_copy) };

    for dev in dev_slice.iter_mut() {
        let opts = &mut dev.opts;
        if opt_defined!(opts, bucket_size) == 0 {
            let clamped = dev_bucket_size_clamp(fs_opts, dev.fs_size, fs_bucket_size);
            opt_set!(opts, bucket_size, clamped as u32);
        }
    }

    for dev in dev_slice.iter_mut() {
        dev.nbuckets = dev.fs_size / dev.opts.bucket_size as u64;
        unsafe { c::bch2_check_bucket_size(fs_opts, dev) };
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
    let mut sb = c::bch_sb_handle::default();
    if unsafe { c::bch2_sb_realloc(&mut sb, 0) } != 0 {
        panic!("insufficient memory");
    }

    let sb_ptr = sb.sb;
    let sb_ref = unsafe { &mut *sb_ptr };

    sb_ref.version = (opts.version as u16).to_le();
    sb_ref.version_min = (opts.version as u16).to_le();
    sb_ref.magic.b = BCHFS_MAGIC;
    sb_ref.user_uuid = opts.uuid;
    sb_ref.nr_devices = devs.nr as u8;

    unsafe {
        c::rust_set_bch_sb_version_incompat_allowed(sb_ptr, opts.version as u64);
        // These are no longer options, only for compatibility with old versions
        c::rust_set_bch_sb_meta_replicas_req(sb_ptr, 1);
        c::rust_set_bch_sb_data_replicas_req(sb_ptr, 1);
        c::rust_set_bch_sb_extent_bp_shift(sb_ptr, 16);
    }

    let version_threshold =
        c::bcachefs_metadata_version::bcachefs_metadata_version_disk_accounting_big_endian as u32;
    if opts.version > version_threshold {
        let features_all = unsafe { c::rust_bch_sb_features_all() };
        sb_ref.features[0] |= features_all.to_le();
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
            &mut sb,
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
            (*m).nbuckets = (dev.nbuckets).to_le();
            (*m).first_bucket = 0;
        }

        {
            let opts = &mut dev.opts;
            if opt_defined!(opts, rotational) == 0 {
                let nonrot = unsafe { c::bdev_nonrot(dev.bdev) };
                opt_set!(opts, rotational, !nonrot as u8);
            }
        }

        opt_set_sb_all(sb.sb, idx as i32, &mut dev.opts);
        unsafe { c::rust_set_bch_member_rotational_set(m, 1) };
    }

    // Disk labels
    for (idx, dev) in dev_slice.iter().enumerate() {
        if dev.label.is_null() {
            continue;
        }

        let path_idx = unsafe { c::bch2_disk_path_find_or_create(&mut sb, dev.label) };
        if path_idx < 0 {
            panic!(
                "error creating disk path: {}",
                std::io::Error::from_raw_os_error(-path_idx)
            );
        }

        // Recompute m after sb modification (memory may have been reallocated)
        let m = unsafe { c::bch2_members_v2_get_mut(sb.sb, idx as i32) };
        unsafe { c::rust_set_bch_member_group(m, path_idx as u64 + 1) };
    }

    // Targets
    let target_strs = unsafe { &fs_opt_strs.__bindgen_anon_1.__bindgen_anon_1 };
    unsafe {
        c::rust_set_bch_sb_foreground_target(
            sb.sb,
            parse_target(&mut sb, dev_slice, target_strs.foreground_target) as u64,
        );
        c::rust_set_bch_sb_background_target(
            sb.sb,
            parse_target(&mut sb, dev_slice, target_strs.background_target) as u64,
        );
        c::rust_set_bch_sb_promote_target(
            sb.sb,
            parse_target(&mut sb, dev_slice, target_strs.promote_target) as u64,
        );
        c::rust_set_bch_sb_metadata_target(
            sb.sb,
            parse_target(&mut sb, dev_slice, target_strs.metadata_target) as u64,
        );
    }

    // Encryption
    if opts.encrypted {
        let crypt_size =
            std::mem::size_of::<c::bch_sb_field_crypt>() / std::mem::size_of::<u64>();
        let crypt = unsafe {
            c::bch2_sb_field_resize_id(
                &mut sb,
                c::bch_sb_field_type::BCH_SB_FIELD_crypt,
                crypt_size as u32,
            ) as *mut c::bch_sb_field_crypt
        };
        unsafe {
            c::bch_sb_crypt_init(sb.sb, crypt, opts.passphrase);
            c::rust_set_bch_sb_encryption_type(sb.sb, 1);
        }
    }

    unsafe { c::bch2_sb_members_cpy_v2_v1(&mut sb) };

    // Write superblocks to each device
    for dev in dev_slice.iter_mut() {
        let size_sectors = dev.fs_size >> 9;
        let sb_ref = unsafe { &mut *sb.sb };
        let dev_idx = unsafe {
            (dev as *const c::dev_opts).offset_from(devs.data) as u8
        };
        sb_ref.dev_idx = dev_idx;

        if dev.sb_offset == 0 {
            dev.sb_offset = c::BCH_SB_SECTOR as u64;
            dev.sb_end = size_sectors;
        }

        unsafe {
            c::bch2_sb_layout_init(
                &mut (*sb.sb).layout,
                fs_opts.block_size as u32,
                dev.opts.bucket_size,
                opts.superblock_size,
                dev.sb_offset,
                dev.sb_end,
                opts.no_sb_at_end,
            );
        }

        if dev.sb_offset == c::BCH_SB_SECTOR as u64 {
            // Zero start of disk
            let zeroes = vec![0u8; (c::BCH_SB_SECTOR as usize) << 9];
            let fd = unsafe { (*dev.bdev).bd_fd };
            let file: std::mem::ManuallyDrop<std::fs::File> =
                std::mem::ManuallyDrop::new(unsafe {
                    std::os::unix::io::FromRawFd::from_raw_fd(fd)
                });
            use std::os::unix::fs::FileExt;
            file.write_all_at(&zeroes, 0)
                .unwrap_or_else(|e| panic!("zeroing start of disk: {}", e));
        }

        let fd = unsafe { (*dev.bdev).bd_fd };
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
fn format_opts_default() -> c::format_opts {
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
        superblock_size: 2048, // SUPERBLOCK_SIZE_DEFAULT
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

const BCHFS_MAGIC: [u8; 16] = [
    0xc6, 0x85, 0x73, 0xf6, 0x66, 0xce, 0x90, 0xa9,
    0xd9, 0x6a, 0x60, 0xcf, 0x80, 0x3d, 0xf7, 0xef,
];
