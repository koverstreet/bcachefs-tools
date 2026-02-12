use std::fs::File;
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use anyhow::{anyhow, bail, Result};
use bch_bindgen::bcachefs;
use bch_bindgen::c;
use bch_bindgen::opt_set;
use bch_bindgen::sb::io as sb_io;
use clap::Parser;

use crate::util::{file_size, parse_human_size};
use bch_bindgen::printbuf::Printbuf;
use crate::wrappers::super_io::{self, BCACHE_MAGIC, BCHFS_MAGIC, SUPERBLOCK_SIZE_DEFAULT};

// bch2_sb_validate's flags parameter is a bch_validate_flags enum in bindgen,
// but C passes 0 (no flags). Since 0 isn't a valid Rust enum variant, declare
// our own FFI binding with the correct ABI type.
extern "C" {
    fn bch2_sb_validate(
        sb: *mut c::bch_sb,
        opts: *mut bcachefs::bch_opts,
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

/// Interpret a byte buffer as a `&bch_sb`.
///
/// SAFETY: `buf` must be large enough to contain a `bch_sb` header and the
/// pointer must be suitably aligned (any 512-byte-aligned buffer suffices).
unsafe fn buf_as_sb(buf: &[u8]) -> &c::bch_sb {
    &*(buf.as_ptr() as *const c::bch_sb)
}

/// Interpret a byte buffer as a `&mut bch_sb`.
///
/// SAFETY: same as `buf_as_sb`.
unsafe fn buf_as_sb_mut(buf: &mut [u8]) -> &mut c::bch_sb {
    &mut *(buf.as_mut_ptr() as *mut c::bch_sb)
}

fn sb_magic_matches(sb: &c::bch_sb) -> bool {
    sb.magic.b == BCACHE_MAGIC || sb.magic.b == BCHFS_MAGIC
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
    let mut opts = bcachefs::bch_opts::default();
    let ret = unsafe { bch2_sb_validate(sb, &mut opts, offset_sectors, 0, err.as_raw()) };
    (ret, err)
}

fn prt_offset(offset: u64) -> Printbuf {
    let mut hr = Printbuf::new();
    hr.human_readable_u64(offset);
    hr
}

fn probe_one_super(dev: &File, sb_size: usize, offset: u64, verbose: bool) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; sb_size];
    let r = dev.read_at(&mut buf, offset).ok()?;
    if r < sb_size {
        return None;
    }

    let sb = unsafe { buf_as_sb_mut(&mut buf) };
    let (ret, _err) = validate_sb(sb, offset >> 9);
    if ret != 0 {
        return None;
    }

    if verbose {
        println!("found superblock at {}", prt_offset(offset));
    }

    let bytes = super_io::vstruct_bytes_sb(unsafe { buf_as_sb(&buf) });
    Some(buf[..bytes].to_vec())
}

fn probe_sb_range(dev: &File, start: u64, end: u64, verbose: bool) -> Vec<Vec<u8>> {
    let start = start & !511u64;
    let end = end & !511u64;
    let buflen = (end - start) as usize;
    let mut buf = vec![0u8; buflen];

    let Ok(r) = dev.read_at(&mut buf, start) else { return Vec::new() };
    if r < buflen {
        return Vec::new();
    }

    let mut results = Vec::new();
    let mut offset = 0usize;

    while offset < buflen {
        let sb = unsafe { buf_as_sb(&buf[offset..]) };

        if !sb_magic_matches(sb) {
            offset += 512;
            continue;
        }

        let bytes = super_io::vstruct_bytes_sb(sb);
        if offset + bytes > buflen {
            eprintln!("found sb {} size {} that overran buffer", start + offset as u64, bytes);
            offset += 512;
            continue;
        }

        let sb = unsafe { buf_as_sb_mut(&mut buf[offset..]) };
        let (ret, err) = validate_sb(sb, (start + offset as u64) >> 9);
        if ret != 0 {
            eprintln!("found sb {} that failed to validate: {}", start + offset as u64, err);
            offset += 512;
            continue;
        }

        if verbose {
            println!("found superblock at {}", prt_offset(start + offset as u64));
        }

        results.push(buf[offset..offset + bytes].to_vec());
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
) -> Result<Vec<u8>> {
    let mut sbs = if offset != 0 {
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
    sbs.sort_by_key(|sb| sb_last_mount_time(unsafe { buf_as_sb(sb) }));
    Ok(sbs.pop().unwrap())
}

fn recover_from_member(src_device: &str, dev_idx: i32, dev_size: u64) -> Result<Vec<u8>> {
    let mut opts = bcachefs::bch_opts::default();
    opt_set!(opts, noexcl, 1);
    opt_set!(opts, nochanges, 1);

    let mut src_sb = sb_io::read_super_opts(Path::new(src_device), opts)
        .map_err(|e| anyhow!("Error opening {}: {}", src_device, e))?;

    let m = unsafe { c::bch2_sb_member_get(src_sb.sb, dev_idx) };
    if m.uuid.b == [0u8; 16] {
        return Err(anyhow!("Member {} does not exist in source superblock", dev_idx));
    }

    unsafe {
        c::bch2_sb_field_delete(&mut src_sb, c::bch_sb_field_type::BCH_SB_FIELD_journal);
        c::bch2_sb_field_delete(&mut src_sb, c::bch_sb_field_type::BCH_SB_FIELD_journal_v2);
        (*src_sb.sb).dev_idx = dev_idx as u8;
    }

    // Read fields safely before layout mutation
    let sb = src_sb.sb();
    let block_size = u16::from_le(sb.block_size) as u32;
    let bucket_size = u16::from_le(m.bucket_size) as u32;
    let sb_max_size = 1u32 << sb.layout.sb_max_size_bits;

    super_io::sb_layout_init(
        unsafe { &mut (*src_sb.sb).layout },
        block_size << 9,
        bucket_size << 9,
        sb_max_size,
        c::BCH_SB_SECTOR as u64,
        dev_size >> 9,
        false,
    );

    // Copy to owned buffer; src_sb's Drop will free the C allocation
    let bytes = super_io::vstruct_bytes_sb(src_sb.sb());
    let sb_buf = unsafe {
        std::slice::from_raw_parts(src_sb.sb as *const u8, bytes).to_vec()
    };

    Ok(sb_buf)
}

pub fn cmd_recover_super(argv: Vec<String>) -> Result<()> {
    let cli = RecoverSuperCli::parse_from(argv);

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
    buf.sb_to_text(
        std::ptr::null_mut(),
        unsafe { buf_as_sb(&sb_buf) },
        true,
        1u32 << c::bch_sb_field_type::BCH_SB_FIELD_members_v2 as u32,
    );
    println!("Found superblock:\n{}", buf);

    if cli.yes {
        println!("Recovering");
    } else {
        print!("Recover? ");
    }

    if cli.yes || unsafe { c::ask_yn() } {
        crate::wrappers::super_io::bch2_super_write(
            dev_file.as_raw_fd(),
            unsafe { buf_as_sb_mut(&mut sb_buf) },
        );
    }

    let _ = std::process::Command::new("udevadm")
        .args(["trigger", "--settle", &cli.device])
        .status();

    if cli.src_device.is_some() {
        println!("Recovered device will no longer have a journal, please run fsck");
    }

    Ok(())
}
