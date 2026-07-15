// SPDX-License-Identifier: GPL-2.0
//
// bcachefs migrate — convert an existing filesystem to bcachefs in place.
//
// Reimplements c_src/cmd_migrate.c in Rust.

use std::ffi::{CString, c_char, c_ulong};
use std::fs;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};

use anyhow::{anyhow, bail, Result};
use bch_bindgen::c;
use bch_bindgen::fs::Fs;
use bch_bindgen::opt_set;
use clap::Parser;

use crate::commands::format::take_opt_value;
use crate::commands::opts::{bch_opt_lookup_negated, parse_opt_val};
use crate::key::Passphrase;
use crate::commands::format_util::{format_opts_default, DevOpts};
use crate::wrappers::super_io;

// ---- C shim declarations ----

extern "C" {
    fn rust_set_bit(nr: c_ulong, addr: *mut c_ulong);
}

use fiemap::FiemapExtentFlags;

/// Iterate over physical extents of a file via FIEMAP ioctl.
fn fiemap_iter(fd: BorrowedFd<'_>) -> Result<Vec<CRange>> {
    let bad_flags = FiemapExtentFlags::UNKNOWN
        | FiemapExtentFlags::ENCODED
        | FiemapExtentFlags::NOT_ALIGNED
        | FiemapExtentFlags::DATA_INLINE;

    let mut ranges = Vec::new();

    for extent in fiemap::Fiemap::new(fd) {
        let e = extent?;
        if e.fe_flags.intersects(bad_flags) {
            bail!("Unable to continue: metadata file not fully mapped");
        }
        ranges.push(CRange { start: e.fe_physical, end: e.fe_physical + e.fe_length });
    }

    ranges_sort_merge(&mut ranges);
    Ok(ranges)
}

// ---- Range utilities ----

/// Matches the C `struct range { u64 start; u64 end; }`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct CRange {
    start: u64,
    end:   u64,
}

fn ranges_sort_merge(ranges: &mut Vec<CRange>) {
    ranges.sort_by_key(|r| r.start);
    let mut merged = Vec::with_capacity(ranges.len());
    for r in ranges.drain(..) {
        if let Some(last) = merged.last_mut() {
            let last: &mut CRange = last;
            if last.end >= r.start {
                last.end = last.end.max(r.end);
                continue;
            }
        }
        merged.push(r);
    }
    *ranges = merged;
}

/// Iterate over holes in the range [0, max) not covered by `ranges`.
/// `ranges` must be sorted and non-overlapping.
fn for_each_hole(ranges: &[CRange], max: u64) -> Vec<CRange> {
    let mut holes = Vec::new();
    let mut pos = 0u64;
    for r in ranges {
        if r.start > pos {
            holes.push(CRange { start: pos, end: r.start });
        }
        pos = r.end;
    }
    if pos < max {
        holes.push(CRange { start: pos, end: max });
    }
    holes
}

// ---- Utility functions ----

/// Resolve a dev_t to a device path via /sys/dev/block/.
fn dev_t_to_path(dev: libc::dev_t) -> Result<String> {
    let major = libc::major(dev);
    let minor = libc::minor(dev);
    let sysfs = format!("/sys/dev/block/{}:{}", major, minor);

    let link = fs::read_link(&sysfs)
        .map_err(|e| anyhow!("readlink error looking up block device {}: {}", sysfs, e))?;

    let name = link.file_name()
        .ok_or_else(|| anyhow!("error looking up device name from {}", link.display()))?;

    Ok(format!("/dev/{}", name.to_string_lossy()))
}

/// Check if a path is a filesystem mount point.
fn path_is_fs_root(path: &str) -> Result<bool> {
    let mountinfo = fs::read_to_string("/proc/self/mountinfo")
        .map_err(|e| anyhow!("Error reading mount information: {}", e))?;

    for line in mountinfo.lines() {
        // Fields: mount_id parent_id dev root mount_point ...
        let mut fields = line.split(' ');
        let _mount_id = fields.next();
        let _parent_id = fields.next();
        let _dev = fields.next();
        let _root = fields.next();
        if let Some(mount_point) = fields.next() {
            if mount_point == path {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// sector_to_bucket: sector / bucket_size
fn sector_to_bucket(bucket_size: u32, sector: u64) -> u64 {
    sector / bucket_size as u64
}

/// bucket_to_sector: bucket * bucket_size
fn bucket_to_sector(bucket_size: u32, bucket: u64) -> u64 {
    bucket * bucket_size as u64
}

/// Mark a range of buckets as nouse.
unsafe fn mark_nouse_range(ca: *const c::bch_dev, sector_from: u64, sector_to: u64) {
    let bucket_size = (*ca).mi.bucket_size as u32;
    let nouse = (*ca).buckets_nouse;
    let mut b = sector_to_bucket(bucket_size, sector_from);
    loop {
        rust_set_bit(b as c_ulong, nouse);
        b += 1;
        if bucket_to_sector(bucket_size, b) >= sector_to {
            break;
        }
    }
}

/// Mark all space NOT covered by the reserved extents as nouse,
/// plus the space for the default superblock layout.
unsafe fn mark_unreserved_space(fs: *mut c::bch_fs, extents: &[CRange]) {
    let ca = (*fs).devs[0];
    let bucket_size = (*ca).mi.bucket_size as u32;
    let nbuckets = (*ca).mi.nbuckets;

    // Byte-addressed max for hole iteration
    let max_bytes = bucket_to_sector(bucket_size, nbuckets) << 9;

    for hole in for_each_hole(extents, max_bytes) {
        if hole.start == hole.end {
            continue;
        }
        mark_nouse_range(
            ca,
            hole.start >> 9,
            (hole.end + (1 << 9) - 1) >> 9,
        );
    }

    // Also mark space for the default sb layout
    let sb_size = 1u64 << (*(*ca).disk_sb.sb).layout.sb_max_size_bits;
    mark_nouse_range(ca, 0, c::BCH_SB_SECTOR as u64 + sb_size * 2);
}

/// Reserve space for bcachefs metadata file, return extents and inode number.
///
/// Tries to reserve as much space as possible on the host filesystem, starting
/// from dev_size and halving on ENOSPC until we get at least 1/10th of the device.
fn reserve_new_fs_space(
    file_path: &str,
    block_size: u32,  // in sectors
    dev_size: u64,
    dev: libc::dev_t,
    force: bool,
) -> Result<(Vec<CRange>, u64)> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let mut open_opts = std::fs::OpenOptions::new();
    open_opts.read(true).write(true).mode(0o600);
    if force {
        open_opts.create(true);
    } else {
        open_opts.create_new(true);
    }
    let file = open_opts.open(file_path)
        .map_err(|e| anyhow!("Error creating {} for bcachefs metadata: {}", file_path, e))?;

    let meta = file.metadata()
        .map_err(|e| anyhow!("fstat: {}", e))?;
    if meta.dev() != dev {
        bail!("bcachefs file has incorrect device");
    }
    let bcachefs_inum = meta.ino();

    let min_size = dev_size / 10;
    let mut size = dev_size;

    loop {
        match rustix::fs::fallocate(
            &file,
            rustix::fs::FallocateFlags::empty(),
            0,
            size,
        ) {
            Ok(()) => break,
            Err(e) => {
                if e != rustix::io::Errno::NOSPC || size <= min_size {
                    bail!("Error reserving space ({} bytes) for bcachefs metadata: {}", size, e);
                }
                size /= 2;
                size = size.max(min_size);
            }
        }
    }
    rustix::fs::fsync(&file).map_err(|e| anyhow!("fsync: {}", e))?;

    let extents = fiemap_iter(file.as_fd())?;

    // Check alignment
    let block_bytes = (block_size as u64) << 9;
    for r in &extents {
        if (r.start & (block_bytes - 1)) != 0 || ((r.end - r.start) & (block_bytes - 1)) != 0 {
            bail!("Unable to continue: unaligned extents in metadata file");
        }
    }

    Ok((extents, bcachefs_inum))
}

/// Find space within the reserved extents for the superblock.
fn find_superblock_space(extents: &[CRange], sb_size: u32, bucket_size: u32) -> Result<(u64, u64)> {
    let bucket_bytes = (bucket_size as u64) << 9;
    let sb_bytes = (sb_size as u64) << 9;

    for r in extents {
        let start = std::cmp::max(256 << 10, r.start)
            .div_ceil(bucket_bytes) * bucket_bytes;
        let end = (r.end / bucket_bytes) * bucket_bytes;

        // Need space for two superblocks
        if start + sb_bytes * 2 <= end {
            return Ok((start >> 9, (start >> 9) + sb_size as u64 * 2));
        }
    }
    bail!("Couldn't find a valid location for superblock");
}

/// Add default superblock layout entries (sector 8 and sector 8+sb_size).
/// Returns the sb_size.
fn add_default_sb_layout(sb: &mut c::bch_sb) -> Result<u32> {
    let sb_size = 1u32 << sb.layout.sb_max_size_bits;
    let bch_sb_sector = c::BCH_SB_SECTOR as u64;

    if sb.layout.nr_superblocks as usize >= sb.layout.sb_offset.len() {
        bail!("Can't add superblock: no space left in superblock layout");
    }

    for i in 0..sb.layout.nr_superblocks as usize {
        let off = u64::from_le(sb.layout.sb_offset[i]);
        if off == bch_sb_sector || off == bch_sb_sector + sb_size as u64 {
            bail!("Superblock layout already has default superblocks");
        }
    }

    // Shift existing entries down by 2
    let nr = sb.layout.nr_superblocks as usize;
    sb.layout.sb_offset.copy_within(0..nr, 2);
    sb.layout.nr_superblocks += 2;
    sb.layout.sb_offset[0] = bch_sb_sector.to_le();
    sb.layout.sb_offset[1] = (bch_sb_sector + sb_size as u64).to_le();

    Ok(sb_size)
}

// ---- Main commands ----

fn migrate_usage() {
    print!("\
bcachefs migrate - migrate an existing filesystem to bcachefs
Usage: bcachefs migrate [OPTION]...

Options:
  -f fs                        Root of filesystem to migrate(s)
      --encrypted              Enable whole filesystem encryption (chacha20/poly1305)
      --no_passphrase          Don't encrypt master encryption key
  -F                           Force, even if metadata file already exists
  -h, --help                   Display this help and exit

Report bugs to <linux-bcachefs@vger.kernel.org>
");
}

fn migrate_fs(
    fs_path: &str,
    fs_opt_strs: c::bch_opt_strs,
    mut fs_opts: c::bch_opts,
    format_opts: c::format_opts,
    force: bool,
) -> Result<()> {
    if !path_is_fs_root(fs_path)? {
        bail!("{} is not a filesystem root", fs_path);
    }

    let fs_path_cstr = CString::new(fs_path)?;
    use std::os::unix::fs::OpenOptionsExt;
    let fs_file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOATIME)
        .open(fs_path)
        .map_err(|e| anyhow!("Error opening {}: {}", fs_path, e))?;

    use std::os::unix::fs::MetadataExt;
    let fs_meta = fs_file.metadata()
        .map_err(|e| anyhow!("fstat: {}", e))?;

    if !fs_meta.is_dir() {
        bail!("{} is not a directory", fs_path);
    }

    let fs_dev = fs_meta.dev();

    // Find the underlying block device
    let dev_path = dev_t_to_path(fs_dev)?;

    // Set up DevOpts and open the device
    let mut c_dev = DevOpts::new(CString::new(dev_path.as_str())?);
    c_dev.open_no_blkid(
        crate::wrappers::bdev::BLK_OPEN_READ | crate::wrappers::bdev::BLK_OPEN_WRITE | crate::wrappers::bdev::BLK_OPEN_BUFFERED,
    ).map_err(|e| {
        anyhow!("Error opening device to format {}: {}", dev_path, io::Error::from_raw_os_error(e))
    })?;

    let file_path = format!("{}/bcachefs", fs_path);
    println!("Creating new filesystem on {} in space reserved at {}", dev_path, file_path);

    let dev_size = crate::wrappers::bdev::get_size(c_dev.fd);
    c_dev.fs_size = dev_size;

    let block_size = crate::commands::format_util::pick_block_size(&fs_opts, std::slice::from_ref(&c_dev));
    opt_set!(fs_opts, block_size, block_size as u16);

    let bucket_size = crate::commands::format_util::pick_bucket_size(&fs_opts, std::slice::from_ref(&c_dev));
    {
        let dev_opts = &mut c_dev.opts;
        opt_set!(dev_opts, bucket_size, bucket_size as u32);
    }
    c_dev.nbuckets = c_dev.fs_size / c_dev.opts.bucket_size as u64;

    crate::commands::format_util::check_bucket_size(&fs_opts, &c_dev);

    // Reserve space for bcachefs metadata — grab as much as we can
    let (extents, bcachefs_inum) = reserve_new_fs_space(
        &file_path,
        block_size >> 9,
        dev_size,
        fs_dev,
        force,
    )?;

    // Find superblock location within reserved extents
    let (sb_offset, sb_end) = find_superblock_space(
        &extents,
        format_opts.superblock_size,
        c_dev.opts.bucket_size,
    )?;
    c_dev.sb_offset = sb_offset;
    c_dev.sb_end = sb_end;

    let sb = crate::commands::format_util::format(fs_opt_strs, fs_opts, format_opts, std::slice::from_mut(&mut c_dev));
    if sb.is_null() {
        bail!("format failed");
    }

    let sb_offset_val = u64::from_le(unsafe { (*sb).layout.sb_offset[0] });

    // Add encryption key if needed
    if !format_opts.passphrase.is_null() {
        let type_cstr = CString::new("user").unwrap();
        let desc_cstr = CString::new("user").unwrap();
        unsafe { c::bch2_add_key(sb, type_cstr.as_ptr(), desc_cstr.as_ptr(), format_opts.passphrase) };
    }

    unsafe { libc::free(sb as *mut _) };

    // Open the new filesystem
    let dev_path_pb = std::path::PathBuf::from(&dev_path);
    let mut opts: c::bch_opts = Default::default();
    opt_set!(opts, sb, sb_offset_val);
    opt_set!(opts, nostart, true as u8);
    opt_set!(opts, noexcl, true as u8);
    opt_set!(opts, reconcile_enabled, false as u8);
    opt_set!(opts, copygc_enabled, false as u8);

    let fs = Fs::open(std::slice::from_ref(&dev_path_pb), opts)
        .map_err(|e| anyhow!("Error opening new filesystem: {}", e))?;

    fs.buckets_nouse_alloc()
        .map_err(|e| anyhow!("Error allocating buckets_nouse: {}", e))?;

    unsafe { mark_unreserved_space(fs.raw, &extents) };

    fs.start()
        .map_err(|e| anyhow!("Error starting new filesystem: {}", e))?;

    // Set no_default_sb feature — prevents rw mount until migrate-superblock is run
    let sb = unsafe { &mut *(*fs.raw).disk_sb.sb };
    sb.features[0] |= (1u64 << c::bch_sb_feature::BCH_FEATURE_no_default_sb as u64).to_le();
    {
        let _lock = fs.sb_lock();
        fs.write_super_ret()
            .map_err(|e| anyhow!("Error writing superblock: {}", e))?;
    }

    // Calculate reserve_start: round up (superblock_size * 2 + BCH_SB_SECTOR) sectors
    // to bucket boundary (in bytes)
    let bucket_bytes = unsafe { (*(*fs.raw).devs[0]).mi.bucket_size as u64 } << 9;
    let reserve_start = (((format_opts.superblock_size as u64 * 2 + c::BCH_SB_SECTOR as u64) << 9)
        .div_ceil(bucket_bytes)) * bucket_bytes;

    // Copy the filesystem tree
    let mut s = crate::copy_fs::CopyFsState::new_migrate(
        bcachefs_inum,
        fs_dev,
        reserve_start,
    );
    // Pre-populate with metadata reservation ranges
    for e in &extents {
        s.extents.push((e.start, e.end));
    }

    let copy_ret = crate::copy_fs::copy_fs(&fs, &mut s, fs_file.as_fd(), &fs_path_cstr);

    let exit_ret = fs.exit();
    drop(fs_file);

    copy_ret.map_err(|e| anyhow!("Error copying filesystem: {}", e))?;
    if exit_ret != 0 {
        bail!("Error closing filesystem (ret={})", exit_ret);
    }

    // Run fsck on the new filesystem
    println!("Migrate complete, running fsck:");
    let mut fsck_opts: c::bch_opts = Default::default();
    opt_set!(fsck_opts, sb, sb_offset_val);
    opt_set!(fsck_opts, noexcl, true as u8);
    opt_set!(fsck_opts, nochanges, true as u8);
    opt_set!(fsck_opts, read_only, true as u8);

    let fs = Fs::open(std::slice::from_ref(&dev_path_pb), fsck_opts)
        .map_err(|e| anyhow!("Error opening new filesystem for fsck: {}", e))?;
    drop(fs);
    println!("fsck complete");

    println!(
        "To mount the new filesystem, run\n\
         \x20 mount -t bcachefs -o sb={sb} {dev} dir\n\
         \n\
         After verifying that the new filesystem is correct, to create a\n\
         superblock at the default offset and finish the migration run\n\
         \x20 bcachefs migrate-superblock -d {dev} -o {sb}\n\
         \n\
         The new filesystem will have a file at /old_migrated_filesystem\n\
         referencing all disk space that might be used by the existing\n\
         filesystem. That file can be deleted once the old filesystem is\n\
         no longer needed (and should be deleted prior to running\n\
         bcachefs migrate-superblock)",
        sb = sb_offset_val,
        dev = dev_path,
    );

    Ok(())
}

fn migrate_superblock(dev_path: &str, sb_offset: u64) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    // Open device and read existing superblock
    let dev_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_EXCL)
        .open(dev_path)
        .map_err(|e| anyhow!("Error opening {}: {}", dev_path, e))?;

    let sb = super_io::__bch2_super_read(dev_file.as_raw_fd(), sb_offset);
    let sb_ref = unsafe { &mut *sb };

    // Validate and add default layout (catches errors early)
    let sb_size = add_default_sb_layout(sb_ref)?;

    // Zero the start of the disk to blow away the old superblock
    let zeroes_len = ((c::BCH_SB_SECTOR as usize) << 9) + std::mem::size_of::<c::bch_sb>();
    let zeroes = vec![0u8; zeroes_len];
    {
        use std::os::unix::fs::FileExt;
        dev_file.write_all_at(&zeroes, 0)
            .map_err(|e| anyhow!("Error zeroing start of disk: {}", e))?;
    }
    drop(dev_file);
    unsafe { libc::free(sb as *mut _) };

    // Open the filesystem with nostart + sb offset
    let dev_path_pb = std::path::PathBuf::from(dev_path);
    let mut opts: c::bch_opts = Default::default();
    opt_set!(opts, nostart, true as u8);
    opt_set!(opts, sb, sb_offset);
    opt_set!(opts, reconcile_enabled, false as u8);
    opt_set!(opts, copygc_enabled, false as u8);

    let fs = Fs::open(&[dev_path_pb], opts)
        .map_err(|e| anyhow!("Error opening filesystem: {}", e))?;

    fs.buckets_nouse_alloc()
        .map_err(|e| anyhow!("Error allocating buckets_nouse: {}", e))?;

    // Mark the new sb bucket range as nouse
    let ca = unsafe { (*fs.raw).devs[0] };
    unsafe { mark_nouse_range(ca, 0, c::BCH_SB_SECTOR as u64 + sb_size as u64 * 2) };

    // Clear no_default_sb feature so recovery doesn't force ro
    unsafe {
        let fs_raw = fs.raw;
        let _lock = fs.sb_lock();
        let sb = &mut *(*fs_raw).disk_sb.sb;
        sb.features[0] &= !(1u64 << c::bch_sb_feature::BCH_FEATURE_no_default_sb as u64).to_le();
        // Also update the cached copy — c->sb.features is host-endian
        (*fs_raw).sb.features &= !(1u64 << c::bch_sb_feature::BCH_FEATURE_no_default_sb as u64);
        fs.write_super_ret()
            .map_err(|e| anyhow!("Error writing superblock: {}", e))?;

        // This is normall set in fs_init if BCH_FEATURE_no_default_sb is
        // cleared - but fs_init already happened:
        (*fs_raw).flags |= (1 as c_ulong) << c::bch_fs_flags::BCH_FS_may_upgrade_downgrade as u32;
    }

    fs.start()
        .map_err(|e| anyhow!("Error starting filesystem: {}", e))?;

    // Verify sb_size consistency
    let actual_sb_size = 1u32 << unsafe { (*(*ca).disk_sb.sb).layout.sb_max_size_bits };
    assert_eq!(actual_sb_size, sb_size, "sb_max_size_bits mismatch after recovery");

    // Apply superblock layout changes (FS is already RW)
    let sb_ref = unsafe { &mut *(*ca).disk_sb.sb };
    add_default_sb_layout(sb_ref)?;

    // Mark the new sb buckets in FS metadata
    let ca_ref = fs.dev_get(0)
        .ok_or_else(|| anyhow!("device 0 not found"))?;
    fs.trans_mark_dev_sb(
        &ca_ref,
        c::btree_iter_update_trigger_flags::BTREE_TRIGGER_transactional,
    ).map_err(|e| anyhow!("Error marking superblock buckets: {}", e))?;
    drop(ca_ref);

    drop(fs);
    Ok(())
}

// ---- Public command entry points ----

fn cmd_migrate(argv: Vec<String>) -> Result<()> {
    let opt_flags = c::opt_flags::OPT_FORMAT as u32;

    let mut fs_path: Option<String> = None;
    let mut encrypted = false;
    let mut no_passphrase = false;
    let mut force = false;

    let mut fs_opts: c::bch_opts = Default::default();
    let mut deferred_opts: Vec<(usize, String)> = Vec::new();

    let mut i = 1;
    while i < argv.len() {
        let arg = &argv[i];

        if arg.starts_with("--") && arg.len() > 2 {
            let opt_part = &arg[2..];
            let (raw_name, inline_val) = match opt_part.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (opt_part, None),
            };
            let name = raw_name.replace('-', "_");

            if let Some((opt_id, opt, negated)) = bch_opt_lookup_negated(&name) {
                if opt.flags as u32 & opt_flags != 0 {
                    let val_str = if negated {
                        "0".to_string()
                    } else if let Some(v) = inline_val {
                        v.to_string()
                    } else if opt.type_ != c::opt_type::BCH_OPT_BOOL {
                        take_opt_value(None, &argv, &mut i, raw_name)?
                    } else {
                        "1".to_string()
                    };

                    match parse_opt_val(opt, &val_str)? {
                        None => deferred_opts.push((opt_id as usize, val_str)),
                        Some(v) => bch_bindgen::opts::opt_set_by_id(&mut fs_opts, opt_id, v),
                    }
                    i += 1;
                    continue;
                }
            }

            match name.as_str() {
                "encrypted" => encrypted = true,
                "no_passphrase" => no_passphrase = true,
                "help" => {
                    migrate_usage();
                    return Ok(());
                }
                _ => bail!("unknown option: {}", arg),
            }

            i += 1;
            continue;
        }

        if arg.starts_with('-') && arg.len() > 1 {
            match arg.as_bytes()[1] {
                b'f' => {
                    i += 1;
                    if i >= argv.len() {
                        bail!("-f requires a value");
                    }
                    fs_path = Some(argv[i].clone());
                }
                b'F' => force = true,
                b'h' => {
                    migrate_usage();
                    return Ok(());
                }
                _ => bail!("unknown option: {}", arg),
            }

            i += 1;
            continue;
        }

        bail!("unexpected argument: {}", arg);
    }

    let fs_path = fs_path.ok_or_else(|| {
        migrate_usage();
        anyhow!("please specify a filesystem to migrate")
    })?;

    // Build format_opts with version detection
    let mut fmt_opts = format_opts_default();
    fmt_opts.encrypted = encrypted;

    // Handle encryption passphrase
    let passphrase: Option<Passphrase> = if encrypted && !no_passphrase {
        Some(Passphrase::new_from_prompt_twice()?)
    } else {
        None
    };

    if let Some(ref p) = passphrase {
        fmt_opts.passphrase = p.get().as_ptr() as *mut c_char;
    }

    // Build bch_opt_strs for deferred options
    let mut fs_opt_strs: c::bch_opt_strs = Default::default();
    for &(id, ref val) in &deferred_opts {
        let cstr = CString::new(val.as_str())?;
        fs_opt_strs.set(id, &cstr);
    }

    let result = migrate_fs(&fs_path, fs_opt_strs, fs_opts, fmt_opts, force);

    fs_opt_strs.free();

    result
}

/// Migrate superblock to standard location
#[derive(Parser, Debug)]
#[command(about = "Create default superblock after migrating")]
pub struct MigrateSuperblockCli {
    /// Device to create superblock for
    #[arg(short = 'd', long = "dev")]
    device: String,

    /// Offset of existing superblock
    #[arg(short = 'o', long = "offset")]
    offset: u64,
}

fn cmd_migrate_superblock(cli: MigrateSuperblockCli) -> Result<()> {
    migrate_superblock(&cli.device, cli.offset)
}

pub const CMD_MIGRATE: super::CmdDef = raw_cmd!("migrate", "Migrate existing filesystem to bcachefs", cmd_migrate);
pub const CMD_MIGRATE_SUPERBLOCK: super::CmdDef = typed_cmd!("migrate-superblock", "Move superblock to standard location", MigrateSuperblockCli, cmd_migrate_superblock);
