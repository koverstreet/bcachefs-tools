use std::ffi::CString;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::process;

use anyhow::{anyhow, Result};
use bch_bindgen::bcachefs;
use bch_bindgen::c;
use bch_bindgen::opt_set;
use clap::Parser;

use crate::util::parse_human_size;
use crate::wrappers::printbuf::Printbuf;

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

// UUID constants — from libbcachefs/bcachefs_format.h
const BCACHE_MAGIC: [u8; 16] = [
    0xc6, 0x85, 0x73, 0xf6, 0x4e, 0x1a, 0x45, 0xca,
    0x82, 0x65, 0xf5, 0x7f, 0x48, 0xba, 0x6d, 0x81,
];
const BCHFS_MAGIC: [u8; 16] = [
    0xc6, 0x85, 0x73, 0xf6, 0x66, 0xce, 0x90, 0xa9,
    0xd9, 0x6a, 0x60, 0xcf, 0x80, 0x3d, 0xf7, 0xef,
];

/// Default superblock size in 512-byte sectors
const SUPERBLOCK_SIZE_DEFAULT: u32 = 2048;

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

/// Total size of a bch_sb structure in bytes (fixed header + variable-length data).
unsafe fn sb_bytes(sb: *const c::bch_sb) -> usize {
    std::mem::size_of::<c::bch_sb>() + (*sb).u64s as usize * 8
}

fn sb_as_ptr(buf: &[u8]) -> *const c::bch_sb { buf.as_ptr() as _ }
fn sb_as_mut_ptr(buf: &mut [u8]) -> *mut c::bch_sb { buf.as_mut_ptr() as _ }

fn sb_magic_matches(sb: *const c::bch_sb) -> bool {
    let magic = unsafe { (*sb).magic.b };
    magic == BCACHE_MAGIC || magic == BCHFS_MAGIC
}

fn sb_last_mount_time(sb: *const c::bch_sb) -> u64 {
    let nr = unsafe { (*sb).nr_devices };
    (0..nr as i32)
        .map(|i| u64::from_le(unsafe { c::bch2_sb_member_get(sb as *mut _, i) }.last_mount as u64))
        .max()
        .unwrap_or(0)
}

fn validate_sb(sb: *mut c::bch_sb, offset_sectors: u64) -> (i32, Printbuf) {
    let mut err = Printbuf::new();
    let mut opts = bcachefs::bch_opts::default();
    let ret = unsafe { bch2_sb_validate(sb, &mut opts, offset_sectors, 0, err.as_raw()) };
    (ret, err)
}

fn prt_offset(offset: u64) -> Printbuf {
    let mut hr = Printbuf::new();
    unsafe { c::bch2_prt_human_readable_u64(hr.as_raw(), offset) };
    hr
}

/// Copy the superblock at `buf[..sb_bytes]` into a new owned Vec.
unsafe fn copy_sb(buf: &[u8]) -> Vec<u8> {
    let bytes = sb_bytes(buf.as_ptr() as _);
    buf[..bytes].to_vec()
}

fn probe_one_super(dev: &File, sb_size: usize, offset: u64, verbose: bool) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; sb_size];
    let r = dev.read_at(&mut buf, offset).ok()?;
    if r < sb_size {
        return None;
    }

    let (ret, _err) = validate_sb(sb_as_mut_ptr(&mut buf), offset >> 9);
    if ret != 0 {
        return None;
    }

    if verbose {
        println!("found superblock at {}", prt_offset(offset));
    }

    Some(unsafe { copy_sb(&buf) })
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
        let sb = unsafe { buf.as_ptr().add(offset) as *const c::bch_sb };

        if !sb_magic_matches(sb) {
            offset += 512;
            continue;
        }

        let bytes = unsafe { sb_bytes(sb) };
        if offset + bytes > buflen {
            eprintln!("found sb {} size {} that overran buffer", start + offset as u64, bytes);
            offset += 512;
            continue;
        }

        let (ret, err) = validate_sb(sb as *mut _, (start + offset as u64) >> 9);
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
) -> Vec<u8> {
    let mut sbs = if offset != 0 {
        probe_one_super(dev, SUPERBLOCK_SIZE_DEFAULT as usize * 512, offset, verbose)
            .into_iter().collect()
    } else {
        let mut v = probe_sb_range(dev, 4096, scan_len, verbose);
        v.extend(probe_sb_range(dev, dev_size - scan_len, dev_size, verbose));
        v
    };

    if sbs.is_empty() {
        eprintln!("Found no bcachefs superblocks");
        process::exit(1);
    }

    // Pick the most recently mounted superblock
    sbs.sort_by_key(|sb| sb_last_mount_time(sb_as_ptr(sb)));
    sbs.pop().unwrap()
}

fn recover_from_member(src_device: &str, dev_idx: i32, dev_size: u64) -> Result<Vec<u8>> {
    let mut opts = bcachefs::bch_opts::default();
    opt_set!(opts, noexcl, 1);
    opt_set!(opts, nochanges, 1);

    let c_path = CString::new(src_device)?;
    let mut src_sb: c::bch_sb_handle = Default::default();
    let ret = unsafe { c::bch2_read_super(c_path.as_ptr(), &mut opts, &mut src_sb) };
    if ret != 0 {
        let err_str = unsafe { std::ffi::CStr::from_ptr(c::bch2_err_str(ret)).to_string_lossy() };
        return Err(anyhow!("Error opening {}: {}", src_device, err_str));
    }

    let m = unsafe { c::bch2_sb_member_get(src_sb.sb, dev_idx) };
    if m.uuid.b == [0u8; 16] {
        unsafe { c::bch2_free_super(&mut src_sb) };
        return Err(anyhow!("Member {} does not exist in source superblock", dev_idx));
    }

    unsafe {
        c::bch2_sb_field_delete(&mut src_sb, c::bch_sb_field_type::BCH_SB_FIELD_journal);
        c::bch2_sb_field_delete(&mut src_sb, c::bch_sb_field_type::BCH_SB_FIELD_journal_v2);
        (*src_sb.sb).dev_idx = dev_idx as u8;
    }

    // Copy to owned buffer, then free the C allocation
    let sb_buf = unsafe {
        let bytes = sb_bytes(src_sb.sb);
        std::slice::from_raw_parts(src_sb.sb as *const u8, bytes).to_vec()
    };
    src_sb.sb = std::ptr::null_mut();
    unsafe { c::bch2_free_super(&mut src_sb) };

    // Set up layout for this device
    let sb = sb_as_ptr(&sb_buf);
    let block_size = unsafe { u16::from_le((*sb).block_size) as u32 };
    let bucket_size = u16::from_le(m.bucket_size) as u32;
    let sb_max_size = unsafe { 1u32 << (*sb).layout.sb_max_size_bits };

    unsafe {
        c::bch2_sb_layout_init(
            &mut (*(sb as *mut c::bch_sb)).layout,
            block_size << 9,
            bucket_size << 9,
            sb_max_size,
            c::BCH_SB_SECTOR as u64,
            dev_size >> 9,
            false,
        );
    }

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
        None => unsafe { c::get_size(dev_file.as_raw_fd()) },
    };

    let mut sb_buf = if let Some(ref src) = cli.src_device {
        recover_from_member(src, cli.dev_idx.unwrap(), dev_size)?
    } else {
        recover_from_scan(&dev_file, dev_size, offset, scan_len, cli.verbose)
    };

    let sb = sb_as_mut_ptr(&mut sb_buf);

    let mut buf = Printbuf::new();
    unsafe {
        c::bch2_sb_to_text(
            buf.as_raw(),
            std::ptr::null_mut(),
            sb,
            true,
            1u32 << c::bch_sb_field_type::BCH_SB_FIELD_members_v2 as u32,
        );
    }
    println!("Found superblock:\n{}", buf);

    if cli.yes {
        println!("Recovering");
    } else {
        print!("Recover? ");
    }

    if cli.yes || unsafe { c::ask_yn() } {
        unsafe { c::bch2_super_write(dev_file.as_raw_fd(), sb) };
    }

    let _ = std::process::Command::new("udevadm")
        .args(["trigger", "--settle", &cli.device])
        .status();

    if cli.src_device.is_some() {
        println!("Recovered device will no longer have a journal, please run fsck");
    }

    Ok(())
}
