use std::fs::File;
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use anyhow::{anyhow, bail, Result};
use bcachefs_kernel::c;
use bcachefs_kernel::opt_set;
use bch_bindgen::sb::io as sb_io;
use bch_bindgen::sb::sb_field_type;
use clap::Parser;

use crate::util::{file_size, parse_human_size};
use bcachefs_kernel::sb::io::{SbBuf, SbParseError, BCACHE_MAGIC, BCHFS_MAGIC};
use bcachefs_kernel::util::printbuf::Printbuf;
use crate::wrappers::super_io::{self, SUPERBLOCK_SIZE_DEFAULT};

// bch2_sb_validate's flags parameter is a bch_validate_flags enum in bindgen,
// but C passes 0 (no flags). Since 0 isn't a valid Rust enum variant, declare
// our own FFI binding with the correct ABI type.
extern "C" {
    fn bch2_sb_validate(
        sb: *mut c::bch_sb,
        opts: *mut c::bch_opts,
        offset: u64,
        flags: u32,
        err: *mut c::printbuf,
    ) -> i32;
}

/// Attempt to recover an overwritten superblock from backups
#[derive(Parser, Debug)]
#[command(about = "Attempt to recover overwritten superblock from backups")]
pub struct RecoverSuperCli {
    /// Size of filesystem on device, in bytes
    #[arg(short = 'd', long = "dev_size")]
    dev_size: Option<String>,

    /// Offset to probe, in bytes (must be a multiple of 512)
    #[arg(short = 'o', long = "offset")]
    offset: Option<String>,

    /// Length in bytes to scan from start and end of device
    #[arg(short = 'l', long = "scan_len")]
    scan_len: Option<String>,

    /// Member device to recover from, in a multi-device fs
    #[arg(short = 's', long = "src_device")]
    src_device: Option<String>,

    /// Index of this device, if recovering from another device
    #[arg(short = 'i', long = "dev_idx")]
    dev_idx: Option<i32>,

    /// Recover without prompting
    #[arg(short = 'y', long = "yes")]
    yes: bool,

    /// Increase logging level
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,

    /// Device to recover
    #[arg(required = true)]
    device: String,
}

fn sb_last_mount_time(sb: &c::bch_sb) -> u64 {
    (0..sb.nr_devices as i32)
        .map(|i| {
            let m = unsafe { c::bch2_sb_member_get(sb as *const _ as *mut _, i) };
            u64::from_le(m.last_mount as u64)
        })
        .max()
        .unwrap_or(0)
}

fn validate_sb(sb: &mut c::bch_sb, offset_sectors: u64) -> (i32, Printbuf) {
    let mut err = Printbuf::new();
    let mut opts = c::bch_opts::default();
    let ret = unsafe { bch2_sb_validate(sb, &mut opts, offset_sectors, 0, err.as_raw()) };
    (ret, err)
}

fn prt_offset(offset: u64) -> Printbuf {
    let mut hr = Printbuf::new();
    hr.human_readable_u64(offset);
    hr
}

fn probe_one_super(dev: &File, sb_size: usize, offset: u64, verbose: bool) -> Option<SbBuf> {
    let mut buf = vec![0u8; sb_size];
    let r = dev.read_at(&mut buf, offset).ok()?;
    if r < sb_size {
        return None;
    }

    let mut sb = SbBuf::from_bytes(&buf).ok()?;
    let (ret, _err) = validate_sb(sb.sb_mut(), offset >> 9);
    if ret != 0 {
        return None;
    }

    if verbose {
        println!("found superblock at {}", prt_offset(offset));
    }

    Some(sb)
}

fn probe_sb_range(dev: &File, start: u64, end: u64, verbose: bool) -> Vec<SbBuf> {
    let start = start & !511u64;
    let end = end & !511u64;
    let buflen = (end - start) as usize;
    let mut buf = vec![0u8; buflen];

    let Ok(r) = dev.read_at(&mut buf, start) else { return Vec::new() };
    if r < buflen {
        return Vec::new();
    }

    let magic_off = std::mem::offset_of!(c::bch_sb, magic);
    let mut results = Vec::new();
    let mut offset = 0usize;

    while offset < buflen {
        /* cheap candidate filter before copying out a full superblock: */
        let magic = buf.get(offset + magic_off..offset + magic_off + 16);
        if magic != Some(&BCACHE_MAGIC) && magic != Some(&BCHFS_MAGIC) {
            offset += 512;
            continue;
        }

        let mut sb = match SbBuf::from_bytes(&buf[offset..]) {
            Ok(sb) => sb,
            Err(e @ (SbParseError::ExtentBeyondBuffer { .. } |
                     SbParseError::FieldBeyondSb { .. } |
                     SbParseError::FieldBadU64s { .. })) => {
                eprintln!("found sb {} {}", start + offset as u64, e);
                offset += 512;
                continue;
            }
            Err(_) => {
                offset += 512;
                continue;
            }
        };

        let (ret, err) = validate_sb(sb.sb_mut(), (start + offset as u64) >> 9);
        if ret != 0 {
            eprintln!("found sb {} that failed to validate: {}", start + offset as u64, err);
            offset += 512;
            continue;
        }

        if verbose {
            println!("found superblock at {}", prt_offset(start + offset as u64));
        }

        results.push(sb);
        offset += 512;
    }

    results
}

fn recover_from_scan(
    dev: &File,
    dev_size: u64,
    offset: u64,
    scan_len: u64,
    verbose: bool,
) -> Result<SbBuf> {
    let mut sbs: Vec<SbBuf> = if offset != 0 {
        probe_one_super(dev, SUPERBLOCK_SIZE_DEFAULT as usize * 512, offset, verbose)
            .into_iter().collect()
    } else {
        let mut v = probe_sb_range(dev, 4096, scan_len, verbose);
        v.extend(probe_sb_range(dev, dev_size - scan_len, dev_size, verbose));
        v
    };

    if sbs.is_empty() {
        bail!("Found no bcachefs superblocks");
    }

    // Pick the most recently mounted superblock
    sbs.sort_by_key(|sb| sb_last_mount_time(sb.sb()));
    Ok(sbs.pop().unwrap())
}

fn recover_from_member(src_device: &str, dev_idx: i32, dev_size: u64) -> Result<SbBuf> {
    let mut opts = c::bch_opts::default();
    opt_set!(opts, noexcl, 1);
    opt_set!(opts, nochanges, 1);

    let mut src_sb = sb_io::read_super_opts(Path::new(src_device), opts)
        .map_err(|e| anyhow!("Error opening {}: {}", src_device, e))?;

    let nr_devices = src_sb.sb().nr_devices as i32;
    if dev_idx < 0 || dev_idx >= nr_devices {
        return Err(anyhow!("Device index {} out of range (filesystem has {} devices)",
                           dev_idx, nr_devices));
    }

    let m = unsafe { c::bch2_sb_member_get(src_sb.sb, dev_idx) };
    if m.uuid.b == [0u8; 16] {
        return Err(anyhow!("Member {} does not exist in source superblock", dev_idx));
    }

    src_sb.field_delete(sb_field_type::journal);
    src_sb.field_delete(sb_field_type::journal_v2);
    src_sb.sb_mut().dev_idx = dev_idx as u8;

    // Read fields safely before layout mutation
    let sb = src_sb.sb();
    let block_size = u16::from_le(sb.block_size) as u32;
    let bucket_size = u16::from_le(m.bucket_size) as u32;
    let sb_max_size = 1u32 << sb.layout.sb_max_size_bits;

    super_io::sb_layout_init(
        &mut src_sb.sb_mut().layout,
        block_size << 9,
        bucket_size << 9,
        sb_max_size,
        c::BCH_SB_SECTOR as u64,
        dev_size >> 9,
        false,
    )?;

    // Copy to an owned buffer; src_sb's Drop will free the C allocation
    SbBuf::from_bytes(src_sb.sb_bytes())
        .map_err(|e| anyhow!("copying superblock from {}: {}", src_device, e))
}

fn cmd_recover_super(cli: RecoverSuperCli) -> Result<()> {

    if cli.src_device.is_some() && cli.dev_idx.is_none() {
        return Err(anyhow!("--src_device requires --dev_idx"));
    }
    if cli.dev_idx.is_some() && cli.src_device.is_none() {
        return Err(anyhow!("--dev_idx requires --src_device"));
    }

    let offset = match &cli.offset {
        Some(s) => {
            let v = parse_human_size(s)?;
            if v & 511 != 0 {
                return Err(anyhow!("offset must be a multiple of 512"));
            }
            v
        }
        None => 0,
    };

    let scan_len = match &cli.scan_len {
        Some(s) => parse_human_size(s)?,
        None => 16 << 20,
    };

    let dev_file = std::fs::OpenOptions::new()
        .read(true).write(true)
        .open(&cli.device)
        .map_err(|e| anyhow!("{}: {}", cli.device, e))?;

    let dev_size = match &cli.dev_size {
        Some(s) => parse_human_size(s)?,
        None => file_size(&dev_file)?,
    };

    let mut sb_buf = if let Some(ref src) = cli.src_device {
        recover_from_member(src, cli.dev_idx.unwrap(), dev_size)?
    } else {
        recover_from_scan(&dev_file, dev_size, offset, scan_len, cli.verbose)?
    };

    let mut buf = Printbuf::new();
    unsafe {
        buf.sb_to_text(
            std::ptr::null_mut(),
            sb_buf.sb(),
            true,
            sb_field_type::members_v2.bit(),
        );
    }
    println!("Found superblock:\n{}", buf);

    if cli.yes {
        println!("Recovering");
    } else {
        print!("Recover? ");
    }

    if cli.yes || unsafe { bch_bindgen::c::ask_yn() } {
        crate::wrappers::super_io::bch2_super_write(dev_file.as_raw_fd(), &mut sb_buf);
    }

    let _ = std::process::Command::new("udevadm")
        .args(["trigger", "--settle", &cli.device])
        .status();

    if cli.src_device.is_some() {
        println!("Recovered device will no longer have a journal, please run fsck");
    }

    Ok(())
}

pub const CMD: super::CmdDef = typed_cmd!("recover-super", "Recover damaged superblock", RecoverSuperCli, cmd_recover_super);
