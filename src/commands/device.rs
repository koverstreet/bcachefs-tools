use std::io::{self, Write};
use std::path::Path;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use bch_bindgen::c::{
    bch_member_state::*,
    bcachefs_metadata_version::bcachefs_metadata_version_reconcile,
    BCH_FORCE_IF_DATA_LOST, BCH_FORCE_IF_DEGRADED, BCH_FORCE_IF_METADATA_LOST,
};
use bch_bindgen::path_to_cstr;
use clap::{Parser, ValueEnum};

use crate::util::{fmt_sectors_human, parse_human_size};
use crate::wrappers::accounting::{data_type_is_empty, data_type_is_hidden};
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::sysfs::bcachefs_kernel_version;

/// Open a filesystem by block device path and return its handle + device index.
fn open_dev(path: &str) -> Result<(BcachefsHandle, u32)> {
    let handle = BcachefsHandle::open(path)
        .with_context(|| format!("opening filesystem for '{}'", path))?;
    let dev_idx = handle.dev_idx();
    if dev_idx < 0 {
        return Err(anyhow!("'{}' does not appear to be a block device member", path));
    }
    Ok((handle, dev_idx as u32))
}

/// Resolve a device identifier (path or numeric index) to a dev_idx
/// on a given filesystem handle.
fn resolve_dev(handle: &BcachefsHandle, dev_str: &str) -> Result<u32> {
    if let Ok(idx) = dev_str.parse::<u32>() {
        return Ok(idx);
    }

    let dev_handle = BcachefsHandle::open(dev_str)
        .with_context(|| format!("opening '{}'", dev_str))?;

    if handle.uuid() != dev_handle.uuid() {
        return Err(anyhow!("{} does not appear to be a member of this filesystem", dev_str));
    }

    let idx = dev_handle.dev_idx();
    if idx < 0 {
        return Err(anyhow!("Could not determine device index for '{}'", dev_str));
    }
    Ok(idx as u32)
}

/// Open a device by path or numeric index, with optional filesystem path.
/// When device is a numeric index, fs_path is required.
fn open_dev_by_path_or_index(device: &str, fs_path: Option<&str>) -> Result<(BcachefsHandle, u32)> {
    if let Some(fs_path) = fs_path {
        let handle = BcachefsHandle::open(fs_path)
            .with_context(|| format!("opening filesystem '{}'", fs_path))?;
        let dev_idx = resolve_dev(&handle, device)?;
        Ok((handle, dev_idx))
    } else if device.parse::<u32>().is_err() {
        open_dev(device)
    } else {
        Err(anyhow!("Filesystem path required when specifying device by index"))
    }
}

#[derive(Parser, Debug)]
#[command(about = "Re-add a device to a running filesystem")]
pub struct OnlineCli {
    /// Device path
    device: String,
}

pub fn cmd_device_online(argv: Vec<String>) -> Result<()> {
    let cli = OnlineCli::parse_from(argv);

    let handle = BcachefsHandle::open(&cli.device)
        .with_context(|| format!("opening filesystem for '{}'", cli.device))?;

    let dev_path = path_to_cstr(&cli.device);
    handle.disk_online(&dev_path)
        .with_context(|| format!("onlining device '{}'", cli.device))
}

#[derive(Parser, Debug)]
#[command(about = "Take a device offline, without removing it")]
pub struct OfflineCli {
    /// Force, if data redundancy will be degraded
    #[arg(short, long)]
    force: bool,

    /// Device path
    device: String,
}

pub fn cmd_device_offline(argv: Vec<String>) -> Result<()> {
    let cli = OfflineCli::parse_from(argv);
    let (handle, dev_idx) = open_dev(&cli.device)?;

    let flags = if cli.force { BCH_FORCE_IF_DEGRADED } else { 0 };
    handle.disk_offline(dev_idx, flags)
        .with_context(|| format!("offlining device '{}'", cli.device))
}

#[derive(Parser, Debug)]
#[command(about = "Remove a device from a filesystem")]
pub struct RemoveCli {
    /// Force removal, even if some data couldn't be migrated
    #[arg(short, long)]
    force: bool,

    /// Force removal, even if some metadata couldn't be migrated
    #[arg(short = 'F', long)]
    force_metadata: bool,

    /// Device path or numeric device index
    device: String,

    /// Filesystem path (required when specifying device by index)
    path: Option<String>,
}

pub fn cmd_device_remove(argv: Vec<String>) -> Result<()> {
    let cli = RemoveCli::parse_from(argv);

    let mut flags = BCH_FORCE_IF_DEGRADED;
    if cli.force {
        flags |= BCH_FORCE_IF_DATA_LOST;
    }
    if cli.force_metadata {
        flags |= BCH_FORCE_IF_METADATA_LOST;
    }

    let (handle, dev_idx) = open_dev_by_path_or_index(
        &cli.device, cli.path.as_deref())?;

    handle.disk_remove(dev_idx, flags)
        .with_context(|| format!("removing device '{}'", cli.device))
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum MemberState {
    Rw,
    Ro,
    Evacuating,
    Spare,
}

impl MemberState {
    fn as_u32(self) -> u32 {
        match self {
            MemberState::Rw         => BCH_MEMBER_STATE_rw as u32,
            MemberState::Ro         => BCH_MEMBER_STATE_ro as u32,
            MemberState::Evacuating => BCH_MEMBER_STATE_evacuating as u32,
            MemberState::Spare      => BCH_MEMBER_STATE_spare as u32,
        }
    }
}

fn block_device_size(dev: &str) -> Result<u64> {
    use std::os::unix::io::AsRawFd;
    let f = std::fs::File::open(dev)
        .with_context(|| format!("opening {}", dev))?;
    let mut size: u64 = 0;
    // BLKGETSIZE64 = _IOR(0x12, 114, size_t)
    const BLKGETSIZE64: libc::c_ulong = 0x80081272;
    let ret = unsafe { libc::ioctl(f.as_raw_fd(), BLKGETSIZE64, &mut size) };
    if ret < 0 {
        return Err(anyhow!("BLKGETSIZE64 failed on {}", dev));
    }
    Ok(size)
}

#[derive(Parser, Debug)]
#[command(about = "Change the state of a device")]
pub struct SetStateCli {
    /// Force if data redundancy will be degraded
    #[arg(short, long)]
    force: bool,

    /// Force even if data will be lost
    #[arg(short = 'F', long)]
    force_if_data_lost: bool,

    /// Device state
    #[arg(value_enum)]
    new_state: MemberState,

    /// Device path or numeric device index
    device: String,

    /// Filesystem path (required when specifying device by index)
    path: Option<String>,
}

pub fn cmd_device_set_state(argv: Vec<String>) -> Result<()> {
    let cli = SetStateCli::parse_from(argv);

    let new_state = cli.new_state.as_u32();
    let mut flags = if cli.force { BCH_FORCE_IF_DEGRADED } else { 0 };
    if cli.force_if_data_lost {
        flags |= BCH_FORCE_IF_DEGRADED | BCH_FORCE_IF_DATA_LOST | BCH_FORCE_IF_METADATA_LOST;
    }

    let (handle, dev_idx) = open_dev_by_path_or_index(
        &cli.device, cli.path.as_deref())?;

    handle.disk_set_state(dev_idx, new_state, flags)
        .context("setting device state")
}

#[derive(Parser, Debug)]
#[command(about = "Resize the filesystem on a device")]
pub struct ResizeCli {
    /// Device path
    device: String,

    /// New size (human-readable, e.g. 1G); defaults to device size
    size: Option<String>,
}

/// Returns Ok(true) if handled, Ok(false) if device not mounted (needs offline path).
pub fn cmd_device_resize(argv: Vec<String>) -> Result<bool> {
    let cli = ResizeCli::parse_from(argv);

    let (handle, dev_idx) = match open_dev(&cli.device) {
        Ok(r) => r,
        Err(_) if Path::new(&cli.device).exists() => return Ok(false),
        Err(e) => return Err(e),
    };

    let size_bytes = match cli.size {
        Some(ref s) => parse_human_size(s)?,
        None => block_device_size(&cli.device)?,
    };
    let size_sectors = size_bytes >> 9;

    let usage = handle.dev_usage(dev_idx)
        .context("querying device usage")?;
    let nbuckets = size_sectors / usage.bucket_size as u64;

    if nbuckets < usage.nr_buckets {
        return Err(anyhow!("Shrinking not supported yet"));
    }

    println!("resizing {} to {} buckets", cli.device, nbuckets);
    handle.disk_resize(dev_idx, nbuckets)
        .context("resizing device")?;
    Ok(true)
}

#[derive(Parser, Debug)]
#[command(about = "Resize the journal on a device")]
pub struct ResizeJournalCli {
    /// Device path
    device: String,

    /// New journal size (human-readable, e.g. 1G)
    size: String,
}

/// Returns Ok(true) if handled, Ok(false) if device not mounted (needs offline path).
pub fn cmd_device_resize_journal(argv: Vec<String>) -> Result<bool> {
    let cli = ResizeJournalCli::parse_from(argv);

    let (handle, dev_idx) = match open_dev(&cli.device) {
        Ok(r) => r,
        Err(_) if Path::new(&cli.device).exists() => return Ok(false),
        Err(e) => return Err(e),
    };

    let size_bytes = parse_human_size(&cli.size)?;
    let size_sectors = size_bytes >> 9;

    let usage = handle.dev_usage(dev_idx)
        .context("querying device usage")?;
    let nbuckets = size_sectors / usage.bucket_size as u64;

    println!("resizing journal on {} to {} buckets", cli.device, nbuckets);
    handle.disk_resize_journal(dev_idx, nbuckets)
        .context("resizing journal")?;
    Ok(true)
}

#[derive(Parser, Debug)]
#[command(about = "Migrate data off a device")]
pub struct EvacuateCli {
    /// Device path
    device: String,
}

pub fn cmd_device_evacuate(argv: Vec<String>) -> Result<()> {
    let cli = EvacuateCli::parse_from(argv);

    if bcachefs_kernel_version() < bcachefs_metadata_version_reconcile as u64 {
        return Err(anyhow!(
            "Kernel too old for Rust evacuate path; \
             need bcachefs metadata version >= {} (reconcile)",
            bcachefs_metadata_version_reconcile as u64
        ));
    }

    let (handle, dev_idx) = open_dev(&cli.device)?;

    let usage = handle.dev_usage(dev_idx)
        .context("querying device usage")?;

    if usage.state == BCH_MEMBER_STATE_rw as u8 {
        println!("Setting {} readonly", cli.device);
        handle.disk_set_state(dev_idx, BCH_MEMBER_STATE_ro as u32, BCH_FORCE_IF_DEGRADED)
            .context("setting device readonly")?;
    }

    println!("Setting {} evacuating", cli.device);
    handle.disk_set_state(dev_idx, BCH_MEMBER_STATE_evacuating as u32, BCH_FORCE_IF_DEGRADED)
        .context("setting device evacuating")?;

    loop {
        let usage = handle.dev_usage(dev_idx)
            .context("querying device usage")?;

        let data_sectors: u64 = usage.iter_typed()
            .filter(|(t, _)| !data_type_is_empty(*t) && !data_type_is_hidden(*t))
            .map(|(_, dt)| dt.sectors)
            .sum();

        print!("\x1b[2K\r{}", fmt_sectors_human(data_sectors));
        io::stdout().flush().ok();

        if data_sectors == 0 {
            println!();
            return Ok(());
        }

        thread::sleep(Duration::from_secs(1));
    }
}
