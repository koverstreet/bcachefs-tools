// SPDX-License-Identifier: GPL-2.0

//! Rust implementation of bch2_format and bch2_format_for_device_add.

use std::ffi::CString;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::io::RawFd;

use bcachefs_kernel::c;
use bcachefs_kernel::alloc::{buckets::bucket, disk_groups::Target};
use bcachefs_kernel::init::dev::{bch_sb_buf, bch_sb_handle};
use bcachefs_kernel::opts::{bch_opts, OptId};
use bcachefs_kernel::sb::members_format::{bch_member, BCH_MIN_NR_NBUCKETS, bch_sb_field_members_v2};
use bcachefs_kernel::os::blkdev;

use crate::util::{die, read_exact_at, write_all_at};
use bcachefs_kernel::format::{MetadataVersion, SbFeatures, bch_encrypted_key, bch_key, bch_sb,
    bch_sb_field_crypt, bch_sb_field_ext, bch_sb_layout, BCH_SB_LAYOUT_SECTOR, BCH_SB_SECTOR};
use bcachefs_kernel::sb::{io::{BCHFS_MAGIC, SbResize}, members::SbMembersMut};
use bcachefs_kernel::util::vstructs::Head;
use bcachefs_kernel::alloc::disk_groups::SbDiskGroupsMut;

use crate::key::Passphrase;

/// What's formatted beside the filesystem's options: its identity, and its
/// superblock's.
#[derive(Clone, Copy, Default)]
pub struct FormatOpts<'a> {
    pub label:           Option<&'a str>,
    /// None: a new one.
    pub uuid:            Option<[u8; 16]>,
    pub version:         MetadataVersion,
    pub superblock_size: u32,
    pub encrypted:       bool,
    /// Encrypted without one, the key's stored as it is.
    pub passphrase:      Option<&'a Passphrase>,
    pub no_sb_at_end:    bool,
}

/// Device options for formatting — Rust replacement for C struct dev_opts.
///
/// Owns the file descriptor and string data.
pub struct DevOpts {
    file: Option<OwnedFd>,
    pub path: CString,
    /// Deferred device options (labels): their values are resolved against
    /// the superblock being built, after it exists.
    pub opt_strs: Vec<(OptId, CString)>,
    pub sb_offset: u64,
    pub sb_end: u64,
    pub nbuckets: u64,
    pub fs_size: u64,
    pub opts: bch_opts,
}

impl DevOpts {
    /// Create a new DevOpts with the given path, no fd yet.
    pub fn new(path: CString) -> Self {
        DevOpts {
            file: None,
            path,
            opt_strs: Vec::new(),
            sb_offset: 0,
            sb_end: 0,
            nbuckets: 0,
            fs_size: 0,
            opts: Default::default(),
        }
    }

    /// The device fd, for interfaces that take raw fds.
    pub fn fd(&self) -> RawFd {
        self.file.as_ref().map_or(-1, |f| f.as_raw_fd())
    }

    /// The device fd; panics if the device hasn't been opened.
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_ref().expect("device not open").as_fd()
    }

    /// Open the device for formatting, with blkid checks.
    ///
    /// Adds READ|WRITE|EXCL|BUFFERED to the given extra flags
    /// (matching the old C open_for_format behavior).
    pub fn open(&mut self, extra_mode: u32, force: bool) -> Result<(), i32> {
        use crate::wrappers::bdev::{blkid_check, open_device};
        let mode = c::BLK_OPEN_READ | c::BLK_OPEN_WRITE | c::BLK_OPEN_EXCL | c::BLK_OPEN_BUFFERED | extra_mode;
        self.file = Some(open_device(&self.path, mode)?);
        blkid_check(self.as_fd(), &self.path, force);
        Ok(())
    }

    /// Open the device without blkid checks or default flags (for migrate).
    pub fn open_no_blkid(&mut self, mode: u32) -> Result<(), i32> {
        self.file = Some(crate::wrappers::bdev::open_device(&self.path, mode)?);
        Ok(())
    }
}

/// Resolve a target string (device path or disk group name) to a target id.
fn parse_target(
    sb: &bch_sb_handle,
    devs: &[DevOpts],
    s: Option<&str>,
) -> u16 {
    let Some(target_str) = s else {
        return 0;
    };
    let target_str_slice = target_str.as_bytes();

    for (idx, dev) in devs.iter().enumerate() {
        if target_str_slice == dev.path.as_bytes() {
            return Target::Dev(idx as u32).encode();
        }
    }

    if let Some(idx) = sb.disk_path_find(target_str_slice) {
        return Target::Group(idx).encode();
    }

    die(&format!("Invalid target {target_str}"));
}

/// Set all sb options from a bch_opts struct.
fn opt_set_sb_all(sb: &mut bch_sb_buf, dev_idx: Option<u32>, opts: &mut bch_opts) {
    use bcachefs_kernel::opts;

    for opt in &opts::OPTS {
        let v = if opts.defined(opt.id) { opts.get(opt.id) } else { opts::DEFAULTS.get(opt.id) };
        opts::opt_set_sb(sb, dev_idx, opt, v, None)
            .unwrap_or_else(|e| die(&format!("error setting {}={v}: {e}", opt.name)));
    }
}

/// Format one or more devices as a bcachefs filesystem.
///
/// Returns the superblock written, or None if a device's layout didn't fit.
///
/// Exits on fatal errors (matching the C `die()` behavior).
pub fn format(
    fs_opt_strs: &[(OptId, String)],
    mut fs_opts: bch_opts,
    opts: FormatOpts<'_>,
    dev_slice: &mut [DevOpts],
) -> Option<bch_sb_handle> {

    // Get device size if not specified (needed for block size threshold)
    for dev in dev_slice.iter_mut() {
        if dev.fs_size == 0 {
            dev.fs_size = blkdev::size(dev.as_fd()).unwrap_or_else(|e| {
                die(&format!("error getting size of {}: {e}", dev.path.to_string_lossy()))
            });
        }
    }

    if !fs_opts.defined(OptId::block_size) {
        let block_size = pick_block_size(&fs_opts, dev_slice);
        fs_opts.set_block_size(block_size as u16);
    }

    if fs_opts.block_size < 512 {
        die(&format!(
            "blocksize too small: {}, must be greater than one sector (512 bytes)",
            fs_opts.block_size
        ));
    }

    if fs_opts.defined(OptId::btree_node_size) && fs_opts.btree_node_size < BTREE_NODE_SIZE_MIN {
        die(&format!(
            "btree node size ({}) too small, must be at least {}",
            fs_opts.btree_node_size, BTREE_NODE_SIZE_MIN
        ));
    }

    // Calculate bucket sizes
    let fs_bucket_size = pick_bucket_size(&fs_opts, dev_slice);

    for dev in dev_slice.iter_mut() {
        let opts = &mut dev.opts;
        if !opts.defined(OptId::bucket_size) {
            let clamped = dev_bucket_size_clamp(fs_opts, dev.fs_size, fs_bucket_size);
            opts.set_bucket_size(clamped as u32);
        }
    }

    for dev in dev_slice.iter_mut() {
        dev.nbuckets = dev.fs_size / dev.opts.bucket_size as u64;
        check_bucket_size(&fs_opts, dev);
    }

    // Calculate btree node size
    if !fs_opts.defined(OptId::btree_node_size) {
        let mut s = bcachefs_kernel::opts::DEFAULTS.btree_node_size;
        for dev in dev_slice.iter() {
            s = s.min(dev.opts.bucket_size);
        }
        fs_opts.set_btree_node_size(s);
    }

    if fs_opts.btree_node_size <= fs_opts.block_size as u32 {
        die(&format!(
            "btree node size ({}) must be greater than block size ({})",
            fs_opts.btree_node_size, fs_opts.block_size
        ));
    }

    let mut sb = bch_sb_handle::default();
    if sb.sb_realloc(0).is_err() {
        die("insufficient memory");
    }

    sb.sb_mut().version.set(opts.version);
    sb.sb_mut().version_min.set(opts.version);
    sb.sb_mut().magic.b = BCHFS_MAGIC;
    sb.sb_mut().user_uuid.b = opts.uuid.unwrap_or_else(|| *uuid::Uuid::new_v4().as_bytes());
    sb.sb_mut().nr_devices = dev_slice.len() as u8;

    sb.sb_mut().set_version_incompat_allowed(opts.version);
    // These are no longer options, only for compatibility with old versions
    sb.sb_mut().set_meta_replicas_req(1);
    sb.sb_mut().set_data_replicas_req(1);
    sb.sb_mut().set_extent_bp_shift(16);

    if opts.version > MetadataVersion::disk_accounting_big_endian {
        sb.sb_mut().features.update(|f| f | SbFeatures::ALL);
    }

    // Internal UUID (different from user_uuid)
    sb.sb_mut().uuid.b = *uuid::Uuid::new_v4().as_bytes();

    // Label
    if let Some(label) = opts.label {
        let label_bytes = label.as_bytes();
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
    let ext_u64s = (std::mem::size_of::<bch_sb_field_ext>() / std::mem::size_of::<u64>()) as u32;
    sb.field_get_minsize::<bch_sb_field_ext>(ext_u64s);

    opt_set_sb_all(&mut sb, None, &mut fs_opts);

    // Time
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_else(|_| die("error getting current time"));
    let nsec = now.as_secs() * 1_000_000_000 + now.subsec_nanos() as u64;
    sb.sb_mut().time_base_lo = nsec.into();
    sb.sb_mut().time_precision = 1u32.into();

    // Member info
    let mi_size = std::mem::size_of::<Head<bch_sb_field_members_v2>>()
        + std::mem::size_of::<bch_member>() * dev_slice.len();
    let mi_u64s = mi_size / std::mem::size_of::<u64>();

    let mi = sb.field_resize::<bch_sb_field_members_v2>(mi_u64s as u32)
        .unwrap_or_else(|| die("failed to resize members_v2 field"));
    mi.member_bytes = (std::mem::size_of::<bch_member>() as u16).into();

    for (idx, dev) in dev_slice.iter_mut().enumerate() {
        let m = sb.member_mut(idx as u32)
            .unwrap_or_else(|| die("member index out of range"));
        m.uuid.b = *uuid::Uuid::new_v4().as_bytes();
        m.nbuckets = dev.nbuckets.into();
        m.first_bucket = 0u16.into();

        let rotational = blkdev::is_rotational(dev.as_fd());
        let opts = &mut dev.opts;
        if !opts.defined(OptId::rotational) {
            opts.set_rotational(rotational);
        }

        opt_set_sb_all(&mut sb, Some(idx as u32), &mut dev.opts);
        sb.member_mut(idx as u32).unwrap().set_rotational_set(true);
    }

    // Deferred device options: labels resolve to a disk path against the sb we
    // just built; the failure domain is a plain string stored in the member.
    for (idx, dev) in dev_slice.iter().enumerate() {
        for (opt_id, val) in &dev.opt_strs {
            match *opt_id {
                OptId::label => {
                    let path_idx = sb.disk_path_find_or_create(val.to_bytes()).unwrap_or_else(|e|
                        die(&format!("error creating disk path {}: {e}", val.to_string_lossy())));
                    // Recompute m after sb modification (may have been reallocated)
                    sb.member_mut(idx as u32).unwrap().set_group((path_idx + 1).try_into().expect("a disk group a member can name"));
                }
                OptId::failure_domain => {
                    let bytes = val.to_bytes();
                    let m = sb.member_mut(idx as u32).unwrap();
                    if bytes.len() > m.failure_domain.len() {
                        die(&format!("failure domain name too long (max {} bytes)",
                                     m.failure_domain.len()));
                    }
                    m.failure_domain.fill(0);
                    m.failure_domain[..bytes.len()].copy_from_slice(bytes);
                }
                _ => die(&format!("can't resolve option {:?} at format time", opt_id)),
            }
        }
    }

    // Targets
    // the last given, as a later option overrides an earlier
    let target_str = |id| fs_opt_strs.iter().rev().find(|(i, _)| *i == id).map(|(_, s)| s.as_str());
    let foreground = parse_target(&sb, dev_slice, target_str(OptId::foreground_target));
    let background = parse_target(&sb, dev_slice, target_str(OptId::background_target));
    let promote    = parse_target(&sb, dev_slice, target_str(OptId::promote_target));
    let metadata   = parse_target(&sb, dev_slice, target_str(OptId::metadata_target));
    sb.sb_mut().set_foreground_target(foreground);
    sb.sb_mut().set_background_target(background);
    sb.sb_mut().set_promote_target(promote);
    sb.sb_mut().set_metadata_target(metadata);

    // Encryption
    if opts.encrypted {
        let crypt_u64s =
            std::mem::size_of::<bch_sb_field_crypt>() / std::mem::size_of::<u64>();
        let nonce = sb.sb().nonce();
        let crypt = sb.field_resize::<bch_sb_field_crypt>(crypt_u64s as u32)
            .unwrap_or_else(|| die("failed to create crypt field"));

        let mut key = bch_key::default();
        bcachefs_kernel::os::kernel::random_bytes(bcachefs_kernel::util::vstructs::plain_bytes_mut(&mut key));
        let mut sb_key = bch_encrypted_key::new_unencrypted(key);

        if let Some(passphrase) = opts.passphrase {
            crypt.kdf_init();
            let passphrase_key = crypt.derive_passphrase(passphrase.get())
                .unwrap_or_else(|e| die(&format!("error deriving key from passphrase: {e}")));
            sb_key.chacha20(&passphrase_key, nonce);
        }
        crypt.key = sb_key;
        sb.sb_mut().set_encryption_type(1);
    }

    sb.members_cpy_v2_v1().unwrap_or_else(|e| die(&format!("copying members to members_v1: {e}")));

    // Write superblocks to each device
    for (idx, dev) in dev_slice.iter_mut().enumerate() {
        let size_sectors = dev.fs_size >> 9;
        sb.sb_mut().dev_idx = idx as u8;

        if dev.sb_offset == 0 {
            dev.sb_offset = BCH_SB_SECTOR as u64;
            dev.sb_end = size_sectors;
        }

        if let Err(e) = sb_layout_init(
            &mut sb.sb_mut().layout,
            fs_opts.block_size as u32,
            dev.opts.bucket_size,
            opts.superblock_size,
            dev.sb_offset,
            dev.sb_end,
            opts.no_sb_at_end,
        ) {
            eprintln!("Error: {}", e);
            return None;
        }

        let fd = dev.as_fd();

        if dev.sb_offset == BCH_SB_SECTOR as u64 {
            // Zero start of disk
            let zeroes = vec![0u8; (BCH_SB_SECTOR as usize) << 9];
            write_all_at(fd, &zeroes, 0)
                .unwrap_or_else(|e| die(&format!("zeroing start of disk: {}", e)));
        }

        bch2_super_write(fd, sb.sb_mut());
    }

    let paths: Vec<&std::ffi::OsStr> = dev_slice
        .iter()
        .map(|dev| std::os::unix::ffi::OsStrExt::from_bytes(dev.path.as_bytes()))
        .collect();
    trigger_udev_for_paths(&paths);

    Some(sb)
}

/// Have udev re-read the devices we just wrote superblocks to, so that
/// 'bcachefs mount' finds them by UUID in its database. Only block devices:
/// an image file has no udev device - udevadm refuses it - and a trigger
/// naming no devices would trigger every device on the system.
pub fn trigger_udev_for_paths<P: AsRef<std::path::Path>>(paths: &[P]) {
    use std::os::unix::fs::FileTypeExt;

    let devs: Vec<&std::path::Path> = paths.iter()
        .map(AsRef::as_ref)
        .filter(|p| std::fs::metadata(p).is_ok_and(|m| m.file_type().is_block_device()))
        .collect();
    if devs.is_empty() {
        return;
    }

    let _ = std::process::Command::new("udevadm")
        .args(["trigger", "--settle"])
        .args(devs)
        .status();
}

/// Format a single device for addition to an existing filesystem.
pub fn format_for_device_add(
    dev: &mut DevOpts,
    block_size: u32,
    btree_node_size: u32,
) -> i32 {
    let mut fs_opts = bch_opts::default();
    fs_opts.set_block_size(block_size as u16);
    fs_opts.set_btree_node_size(btree_node_size);

    let fmt_opts = format_opts_default();
    format(&[], fs_opts, fmt_opts, std::slice::from_mut(dev));

    0
}

/// The newest version the running kernel mounts: ours, if there's no module.
pub(crate) fn kernel_format_version() -> MetadataVersion {
    // Try to load bcachefs module to detect kernel version
    let _ = std::process::Command::new("modprobe")
        .arg("bcachefs")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    let current = MetadataVersion::CURRENT;
    crate::wrappers::sysfs::bcachefs_kernel_version().map_or(current, |k| current.min(k))
}

/// The newest version the running kernel mounts, and the default superblock.
pub(crate) fn format_opts_default() -> FormatOpts<'static> {
    FormatOpts {
        version: kernel_format_version(),
        superblock_size: SUPERBLOCK_SIZE_DEFAULT,
        ..Default::default()
    }
}

/*
 * Every small bset write takes a whole block: with fewer blocks than this per
 * node, nodes fill faster than they can be compacted - and compacting one
 * allocates, which dirties more nodes. Going read-only, flushing the journal
 * never finishes.
 */
const BTREE_NODE_SIZE_MIN: u32 = 64 << 10;

/// The btree node size buckets must hold: the one given, or the smallest
/// allowed.
fn btree_node_size_min(opts: &bch_opts) -> u64 {
    if opts.defined(OptId::btree_node_size) {
        opts.btree_node_size as u64
    } else {
        BTREE_NODE_SIZE_MIN as u64
    }
}

/// Clamp fs-wide bucket size for a specific device that may be too small.
///
/// Prefer at least 2048 buckets per device (512 is the absolute minimum
/// but gets dicey). Within that constraint, try to reach at least
/// encoded_extent_max to avoid fragmenting checksummed/compressed extents.
fn dev_bucket_size_clamp(fs_opts: bch_opts, dev_size: u64, fs_bucket_size: u64) -> u64 {
    let min_nr_nbuckets = BCH_MIN_NR_NBUCKETS as u64;

    // Largest bucket size that still gives >= 2048 buckets
    let max_size = rounddown_pow_of_two(dev_size / (min_nr_nbuckets * 4))
        .max(btree_node_size_min(&fs_opts));
    if max_size * min_nr_nbuckets > dev_size {
        die(&format!("bucket size {} too big for device size", max_size));
    }

    let mut dev_bucket_size = max_size.min(fs_bucket_size);

    // Buckets >= encoded_extent_max avoid fragmenting encoded extents
    let extent_min = fs_opts.encoded_extent_max_or_default() as u64;
    while dev_bucket_size < extent_min && dev_bucket_size < max_size {
        dev_bucket_size *= 2;
    }

    dev_bucket_size
}

fn total_system_ram() -> u64 {
    let info = rustix::system::sysinfo();
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
pub fn pick_block_size(_fs_opts: &bch_opts, dev_slice: &[DevOpts]) -> u32 {
    let total_size: u64 = dev_slice.iter().map(|d| d.fs_size).sum();

    let block_size = if total_size >= 1u64 << 30 {
        let mut bs = 4096u32;
        for dev in dev_slice.iter() {
            bs = bs.max(crate::wrappers::bdev::get_blocksize_physical_hint(dev.as_fd()));
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
pub fn pick_bucket_size(opts: &bch_opts, devs: &[DevOpts]) -> u64 {
    // Hard minimum: bucket must hold a btree node
    let mut bucket_size = (opts.block_size as u64).max(btree_node_size_min(opts));

    let min_dev_size = BCH_MIN_NR_NBUCKETS as u64 * bucket_size;
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
    bucket_size = bucket_size.max(opts.encoded_extent_max_or_default() as u64 * 4);

    // Prefer larger buckets up to 2MB — reduces allocator overhead.
    // Scales linearly with total filesystem size, reaching 2MB at 2TB
    let perf_lower_bound = (2u64 << 20).min(total_fs_size / (1u64 << 20));
    bucket_size = bucket_size.max(perf_lower_bound);

    // Upper bound on bucket count: ensure we can fsck with available
    // memory. Large fudge factor to allow for other fsck processes
    // and devices being added after creation
    let total_ram = total_system_ram();
    let mem_available_for_fsck = total_ram / 8;
    let bucket_struct_size = std::mem::size_of::<bucket>() as u64;
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
pub fn check_bucket_size(opts: &bch_opts, dev: &DevOpts) {
    if dev.opts.bucket_size < opts.block_size as u32 {
        die(&format!(
            "Bucket size ({}) cannot be smaller than block size ({})",
            dev.opts.bucket_size, opts.block_size
        ));
    }

    if (dev.opts.bucket_size as u64) < btree_node_size_min(opts) {
        die(&format!(
            "Bucket size ({}) cannot be smaller than {}btree node size ({})",
            dev.opts.bucket_size,
            if opts.defined(OptId::btree_node_size) { "" } else { "the minimum " },
            btree_node_size_min(opts)
        ));
    }

    if dev.nbuckets < BCH_MIN_NR_NBUCKETS as u64 {
        die(&format!(
            "Not enough buckets: {}, need {} (bucket size {})",
            dev.nbuckets,
            BCH_MIN_NR_NBUCKETS,
            dev.opts.bucket_size
        ));
    }
}

/// Write superblock to all layout locations on disk.
///
/// Exits on I/O errors (matches C `die()` behavior).
pub fn bch2_super_write(fd: BorrowedFd<'_>, sb: &mut bch_sb) {
    let bs = crate::wrappers::bdev::get_blocksize_physical_hint(fd) as usize;
    let layout_range = {
        let off = std::mem::offset_of!(Head<bch_sb>, layout);
        off..off + std::mem::size_of::<bch_sb_layout>()
    };

    let nr_superblocks = sb.layout.nr_superblocks as usize;
    for i in 0..nr_superblocks {
        let offset_le = sb.layout.sb_offset[i];
        let offset_sectors = offset_le.get();

        sb.offset = offset_le;
        sb.csum = sb.as_unvalidated().checksum();

        let sb_src = sb.as_bytes();
        let sb_bytes = sb_src.len();
        let layout_src = &sb_src[layout_range.clone()];

        if offset_sectors == BCH_SB_SECTOR as u64 && bs > 4096 {
            // Layout and superblock are in the same aligned block;
            // write them together.
            let layout_offset = (BCH_SB_LAYOUT_SECTOR as usize) << 9;
            let sb_offset = (offset_sectors as usize) << 9;
            let write_len = (sb_offset + sb_bytes).next_multiple_of(bs);
            let mut buf = vec![0u8; write_len];

            buf[layout_offset..layout_offset + layout_src.len()].copy_from_slice(layout_src);
            buf[sb_offset..sb_offset + sb_bytes].copy_from_slice(sb_src);

            pwrite_exact(fd, &buf, 0);
        } else {
            if offset_sectors == BCH_SB_SECTOR as u64 {
                // Write backup layout in the block preceding the superblock
                let mut buf = vec![0u8; bs];

                read_exact_at(fd, &mut buf, 4096 - bs as u64)
                    .unwrap_or_else(|e| die(&format!("pread failed at offset {}: {}", 4096 - bs, e)));

                buf[bs - layout_src.len()..].copy_from_slice(layout_src);

                pwrite_exact(fd, &buf, 4096 - bs as u64);
            }

            let write_len = sb_bytes.next_multiple_of(bs);
            let mut buf = vec![0u8; write_len];
            buf[..sb_bytes].copy_from_slice(sb_src);

            pwrite_exact(fd, &buf, offset_sectors << 9);
        }
    }

    if let Err(e) = rustix::fs::fsync(fd) {
        die(&format!("fsync failed writing superblock: {}", e));
    }
}

/// Write exactly `buf.len()` bytes at `offset`. Exits on error.
fn pwrite_exact(fd: BorrowedFd<'_>, buf: &[u8], offset: u64) {
    write_all_at(fd, buf, offset)
        .unwrap_or_else(|e| die(&format!("pwrite failed at offset {}: {}", offset, e)));
}

/// Default superblock size in 512-byte sectors
pub const SUPERBLOCK_SIZE_DEFAULT: u32 = 2048;

/// Initialize superblock layout with primary and backup superblock positions.
///
/// `block_size` and `bucket_size` are in bytes.
/// `sb_size`, `sb_start`, and `sb_end` are in 512-byte sectors.
pub fn sb_layout_init(
    l: &mut bch_sb_layout,
    block_size: u32,
    bucket_size: u32,
    sb_size: u32,
    sb_start: u64,
    sb_end: u64,
    no_sb_at_end: bool,
) -> anyhow::Result<()> {
    *l = pin_init::zeroed();

    l.magic.b = bcachefs_kernel::sb::io::BCHFS_MAGIC;
    l.layout_type = 0;
    l.nr_superblocks = 2;
    l.sb_max_size_bits = sb_size.ilog2() as u8;

    // Create two superblocks in the allowed range
    let mut sb_pos = sb_start;
    for i in 0..l.nr_superblocks as usize {
        if sb_pos != BCH_SB_SECTOR as u64 {
            let align = (block_size >> 9) as u64;
            sb_pos = sb_pos.next_multiple_of(align);
        }

        l.sb_offset[i] = sb_pos.into();
        sb_pos += sb_size as u64;
    }

    if sb_pos > sb_end {
        return Err(anyhow::anyhow!(
            "insufficient space for superblocks: need {} sectors but only {} available",
            sb_pos - sb_start, sb_end - sb_start
        ));
    }

    // Also create a backup superblock at the end of the disk:
    //
    // If we're not creating a superblock at the default offset, it
    // means we're being run from the migrate tool and we could be
    // overwriting existing data if we write to the end of the disk
    if sb_start == BCH_SB_SECTOR as u64 && !no_sb_at_end {
        let sb_max_size = 1u64 << l.sb_max_size_bits;
        let bucket_sectors = (bucket_size >> 9) as u64;
        let backup_sb = (sb_end - sb_max_size) / bucket_sectors * bucket_sectors;
        let idx = l.nr_superblocks as usize;
        l.sb_offset[idx] = backup_sb.into();
        l.nr_superblocks += 1;
    }

    Ok(())
}
