use std::ffi::CString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use bch_bindgen::c::{
    self,
    bch_degraded_actions,
    bch_member_state::*,
    bcachefs_metadata_version::bcachefs_metadata_version_reconcile,
    BCH_FORCE_IF_DATA_LOST, BCH_FORCE_IF_DEGRADED, BCH_FORCE_IF_METADATA_LOST,
};
use bch_bindgen::fs::Fs;
use bch_bindgen::opt_set;
use bch_bindgen::path_to_cstr;
use clap::{Arg, ArgAction, Command, Parser, ValueEnum};

use crate::commands::opts::{bch_opt_lookup, bch_option_args, bch_options_from_matches, parse_opt_val};
use crate::util::{fmt_sectors_human, parse_human_size};
use crate::wrappers::accounting::{data_type_is_empty, data_type_is_hidden};
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::sysfs::{self, bcachefs_kernel_version};

fn device_add_opt_flags() -> u32 {
    c::opt_flags::OPT_FORMAT as u32 | c::opt_flags::OPT_DEVICE as u32
}

pub fn device_add_cmd() -> Command {
    Command::new("add")
        .about("Add a new device to an existing filesystem")
        .args(bch_option_args(device_add_opt_flags()))
        .arg(Arg::new("label")
            .short('l')
            .long("label")
            .help("Disk label"))
        .arg(Arg::new("force")
            .short('f')
            .long("force")
            .action(ArgAction::SetTrue)
            .help("Use device even if it appears to already be formatted"))
        .arg(Arg::new("filesystem")
            .required(true)
            .help("Filesystem path or mountpoint"))
        .arg(Arg::new("device")
            .required(true)
            .help("Device to add"))
}

pub fn cmd_device_add(argv: Vec<String>) -> Result<()> {
    let matches = device_add_cmd().get_matches_from(argv);

    let fs_path = matches.get_one::<String>("filesystem").unwrap();
    let dev_path = matches.get_one::<String>("device").unwrap();
    let label = matches.get_one::<String>("label");
    let force = matches.get_flag("force");

    let handle = BcachefsHandle::open(fs_path)
        .map_err(|e| anyhow!("opening filesystem '{}': {}", fs_path, e))?;

    let block_size = parse_human_size(
        &sysfs::read_sysfs_fd_str(handle.sysfs_fd(), "options/block_size")
            .context("reading block_size from sysfs")?,
    ).context("parsing block_size")?;
    let btree_node_size = parse_human_size(
        &sysfs::read_sysfs_fd_str(handle.sysfs_fd(), "options/btree_node_size")
            .context("reading btree_node_size from sysfs")?,
    ).context("parsing btree_node_size")?;

    // Build dev_opts with bch_opts from parsed arguments
    let mut dev_opts: c::dev_opts = Default::default();

    let c_path = CString::new(dev_path.as_str())?;
    dev_opts.path = c_path.as_ptr();

    let c_label = label.map(|l| CString::new(l.as_str())).transpose()?;
    if let Some(ref l) = c_label {
        dev_opts.label = l.as_ptr();
    }

    // Apply bcachefs options (--discard, --durability, etc.)
    let bch_opts = bch_options_from_matches(&matches, device_add_opt_flags());
    for (name, value) in &bch_opts {
        let Some((opt_id, opt)) = bch_opt_lookup(name) else { continue };
        let val = parse_opt_val(opt, value)?
            .ok_or_else(|| anyhow!("option {} requires open filesystem", name))?;
        unsafe { c::bch2_opt_set_by_id(&mut dev_opts.opts, opt_id, val) };
    }

    let ret = unsafe { c::open_for_format(&mut dev_opts, 0, force) };
    if ret != 0 {
        return Err(anyhow!("error opening {}: {}",
            dev_path, std::io::Error::from_raw_os_error(-ret)));
    }

    let ret = crate::wrappers::format::bch2_format_for_device_add(
        &mut dev_opts, block_size as u32, btree_node_size as u32,
    );
    if ret != 0 {
        return Err(anyhow!("error formatting {}: {}",
            dev_path, std::io::Error::from_raw_os_error(-ret)));
    }

    let c_dev_path = path_to_cstr(dev_path);
    handle.disk_add(&c_dev_path)
        .map_err(|e| anyhow!("adding device '{}': {}", dev_path, e))?;

    Ok(())
}

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

fn device_size(dev: &str) -> Result<u64> {
    let f = std::fs::File::open(dev)
        .with_context(|| format!("opening {}", dev))?;
    crate::util::file_size(&f)
        .with_context(|| format!("getting size of {}", dev))
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

    /// Set state of an offline device
    #[arg(short = 'o', long)]
    offline: bool,

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

    if cli.offline {
        if cli.device.parse::<u32>().is_ok() {
            return Err(anyhow!("Cannot specify offline device by id"));
        }
        return set_state_offline(&cli.device, new_state);
    }

    let mut flags = if cli.force { BCH_FORCE_IF_DEGRADED } else { 0 };
    if cli.force_if_data_lost {
        flags |= BCH_FORCE_IF_DEGRADED | BCH_FORCE_IF_DATA_LOST | BCH_FORCE_IF_METADATA_LOST;
    }

    let (handle, dev_idx) = open_dev_by_path_or_index(
        &cli.device, cli.path.as_deref())?;

    handle.disk_set_state(dev_idx, new_state, flags)
        .context("setting device state")
}

fn set_state_offline(device: &str, new_state: u32) -> Result<()> {
    use crate::wrappers::bch_err_str;

    let c_path = CString::new(device)?;
    let mut opts: c::bch_opts = Default::default();
    opt_set!(opts, nostart, 1);
    opt_set!(opts, degraded, bch_degraded_actions::BCH_DEGRADED_very as u8);

    // Read superblock to get dev_idx
    let mut sb_handle: c::bch_sb_handle = Default::default();
    let ret = unsafe { c::bch2_read_super(c_path.as_ptr(), &mut opts, &mut sb_handle) };
    if ret != 0 {
        return Err(anyhow!("error opening {}: {}", device, bch_err_str(ret)));
    }
    let dev_idx = unsafe { (*sb_handle.sb).dev_idx as u32 };
    unsafe { c::bch2_free_super(&mut sb_handle) };

    let fs = Fs::open(&[PathBuf::from(device)], opts)
        .map_err(|e| anyhow!("Error opening filesystem: {}", e))?;

    unsafe { c::rust_device_set_state_offline(fs.raw, dev_idx, new_state) };
    Ok(())
}

#[derive(Parser, Debug)]
#[command(about = "Resize the filesystem on a device")]
pub struct ResizeCli {
    /// Device path
    device: String,

    /// New size (human-readable, e.g. 1G); defaults to device size
    size: Option<String>,
}

pub fn cmd_device_resize(argv: Vec<String>) -> Result<()> {
    let cli = ResizeCli::parse_from(argv);

    let size_bytes = match cli.size {
        Some(ref s) => parse_human_size(s)?,
        None => device_size(&cli.device)?,
    };
    let size_sectors = size_bytes >> 9;

    match open_dev(&cli.device) {
        Ok((handle, dev_idx)) => {
            println!("Doing online resize of {}", cli.device);

            let usage = handle.dev_usage(dev_idx)
                .context("querying device usage")?;
            let nbuckets = size_sectors / usage.bucket_size as u64;

            if nbuckets < usage.nr_buckets {
                return Err(anyhow!("Shrinking not supported yet"));
            }

            println!("resizing {} to {} buckets", cli.device, nbuckets);
            handle.disk_resize(dev_idx, nbuckets)
                .context("resizing device")?;
        }
        Err(_) if Path::new(&cli.device).exists() => {
            println!("Doing offline resize of {}", cli.device);
            resize_offline(&cli.device, size_sectors)?;
        }
        Err(e) => return Err(e),
    }

    Ok(())
}

fn resize_offline(device: &str, size_sectors: u64) -> Result<()> {
    use crate::wrappers::bch_err_str;

    let opts: c::bch_opts = Default::default();
    let fs = Fs::open(&[PathBuf::from(device)], opts)
        .map_err(|e| anyhow!("error opening {}: {}", device, e))?;

    let ret = unsafe { c::rust_device_resize_offline(fs.raw, size_sectors) };
    if ret != 0 {
        return Err(anyhow!("resize error: {}", bch_err_str(ret)));
    }
    Ok(())
}

#[derive(Parser, Debug)]
#[command(about = "Resize the journal on a device")]
pub struct ResizeJournalCli {
    /// Device path
    device: String,

    /// New journal size (human-readable, e.g. 1G)
    size: String,
}

pub fn cmd_device_resize_journal(argv: Vec<String>) -> Result<()> {
    let cli = ResizeJournalCli::parse_from(argv);

    let size_bytes = parse_human_size(&cli.size)?;
    let size_sectors = size_bytes >> 9;

    match open_dev(&cli.device) {
        Ok((handle, dev_idx)) => {
            let usage = handle.dev_usage(dev_idx)
                .context("querying device usage")?;
            let nbuckets = size_sectors / usage.bucket_size as u64;

            println!("resizing journal on {} to {} buckets", cli.device, nbuckets);
            handle.disk_resize_journal(dev_idx, nbuckets)
                .context("resizing journal")?;
        }
        Err(_) if Path::new(&cli.device).exists() => {
            println!("{} is offline - starting:", cli.device);
            resize_journal_offline(&cli.device, size_sectors)?;
        }
        Err(e) => return Err(e),
    }

    Ok(())
}

fn resize_journal_offline(device: &str, size_sectors: u64) -> Result<()> {
    use crate::wrappers::bch_err_str;

    let opts: c::bch_opts = Default::default();
    let fs = Fs::open(&[PathBuf::from(device)], opts)
        .map_err(|e| anyhow!("error opening {}: {}", device, e))?;

    let ret = unsafe { c::rust_device_resize_journal_offline(fs.raw, size_sectors) };
    if ret != 0 {
        return Err(anyhow!("resize error: {}", bch_err_str(ret)));
    }
    Ok(())
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
