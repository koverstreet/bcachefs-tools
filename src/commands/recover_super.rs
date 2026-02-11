use std::ffi::CString;
use std::process;

use anyhow::{anyhow, Result};
use bch_bindgen::bcachefs;
use bch_bindgen::c;
use bch_bindgen::opt_set;
use clap::Parser;

use crate::wrappers::printbuf::Printbuf;

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

fn parse_size(s: &str) -> Result<u64> {
    let mut val: u64 = 0;
    let ret = unsafe { c::bch2_strtoull_h(CString::new(s)?.as_ptr(), &mut val) };
    if ret != 0 {
        return Err(anyhow!("invalid size: {}", s));
    }
    Ok(val)
}

/// Calculate the total size of a bch_sb structure (fixed header + variable-length data)
unsafe fn sb_bytes(sb: *const c::bch_sb) -> usize {
    std::mem::size_of::<c::bch_sb>() + (*sb).u64s as usize * 8
}

fn sb_magic_matches(sb: *const c::bch_sb) -> bool {
    let magic = unsafe { (*sb).magic.b };
    magic == BCACHE_MAGIC || magic == BCHFS_MAGIC
}

fn sb_last_mount_time(sb: *mut c::bch_sb) -> u64 {
    let nr = unsafe { (*sb).nr_devices };
    let mut best = 0u64;
    for i in 0..nr as i32 {
        let m = unsafe { c::bch2_sb_member_get(sb, i) };
        let t = u64::from_le(m.last_mount as u64);
        if t > best {
            best = t;
        }
    }
    best
}

fn probe_one_super(
    dev_fd: i32,
    sb_size: usize,
    offset: u64,
    verbose: bool,
) -> Option<*mut c::bch_sb> {
    let mut buf = vec![0u8; sb_size];
    let r = unsafe {
        libc::pread(dev_fd, buf.as_mut_ptr() as *mut libc::c_void, sb_size, offset as i64)
    };
    if r < sb_size as isize {
        return None;
    }

    let sb = buf.as_ptr() as *mut c::bch_sb;
    let mut err = Printbuf::new();
    let mut opts = bcachefs::bch_opts::default();
    let ret = unsafe {
        c::bch2_sb_validate(sb, &mut opts, offset >> 9, std::mem::transmute(0u32), err.as_raw())
    };

    if ret != 0 {
        return None;
    }

    if verbose {
        let mut hr = Printbuf::new();
        unsafe { c::bch2_prt_human_readable_u64(hr.as_raw(), offset) };
        println!("found superblock at {}", hr);
    }

    // Allocate a copy that outlives buf
    let bytes = unsafe { sb_bytes(sb) };
    let copy = unsafe {
        let p = libc::malloc(bytes) as *mut u8;
        std::ptr::copy_nonoverlapping(buf.as_ptr(), p, bytes);
        p as *mut c::bch_sb
    };
    Some(copy)
}

fn probe_sb_range(
    dev_fd: i32,
    start: u64,
    end: u64,
    verbose: bool,
) -> Vec<*mut c::bch_sb> {
    let start = start & !511u64;
    let end = end & !511u64;
    let buflen = (end - start) as usize;
    let mut buf = vec![0u8; buflen];

    let r = unsafe {
        libc::pread(dev_fd, buf.as_mut_ptr() as *mut libc::c_void, buflen, start as i64)
    };
    if r < buflen as isize {
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
            eprintln!(
                "found sb {} size {} that overran buffer",
                start + offset as u64,
                bytes
            );
            offset += 512;
            continue;
        }

        let mut err = Printbuf::new();
        let mut opts = bcachefs::bch_opts::default();
        let ret = unsafe {
            c::bch2_sb_validate(
                sb as *mut c::bch_sb,
                &mut opts,
                (start + offset as u64) >> 9,
                std::mem::transmute(0u32),
                err.as_raw(),
            )
        };

        if ret != 0 {
            eprintln!(
                "found sb {} that failed to validate: {}",
                start + offset as u64,
                err
            );
            offset += 512;
            continue;
        }

        if verbose {
            let mut hr = Printbuf::new();
            unsafe { c::bch2_prt_human_readable_u64(hr.as_raw(), start + offset as u64) };
            println!("found superblock at {}", hr);
        }

        let copy = unsafe {
            let p = libc::malloc(bytes) as *mut u8;
            std::ptr::copy_nonoverlapping(buf.as_ptr().add(offset), p, bytes);
            p as *mut c::bch_sb
        };
        results.push(copy);

        offset += 512;
    }

    results
}

fn recover_from_scan(
    dev_fd: i32,
    dev_size: u64,
    offset: u64,
    scan_len: u64,
    verbose: bool,
) -> *mut c::bch_sb {
    let sbs = if offset != 0 {
        let mut v = Vec::new();
        if let Some(sb) = probe_one_super(dev_fd, SUPERBLOCK_SIZE_DEFAULT as usize * 512, offset, verbose) {
            v.push(sb);
        }
        v
    } else {
        let mut v = probe_sb_range(dev_fd, 4096, scan_len, verbose);
        v.extend(probe_sb_range(dev_fd, dev_size - scan_len, dev_size, verbose));
        v
    };

    if sbs.is_empty() {
        eprintln!("Found no bcachefs superblocks");
        process::exit(1);
    }

    // Pick the most recently mounted superblock
    let mut best_idx = 0;
    for (i, sb) in sbs.iter().enumerate() {
        if sb_last_mount_time(*sb) > sb_last_mount_time(sbs[best_idx]) {
            best_idx = i;
        }
    }

    let best = sbs[best_idx];

    // Free the rest
    for (i, sb) in sbs.iter().enumerate() {
        if i != best_idx {
            unsafe { libc::free(*sb as *mut libc::c_void) };
        }
    }

    best
}

fn recover_from_member(
    src_device: &str,
    dev_idx: i32,
    dev_size: u64,
) -> Result<*mut c::bch_sb> {
    let mut opts = bcachefs::bch_opts::default();
    opt_set!(opts, noexcl, 1);
    opt_set!(opts, nochanges, 1);

    let c_path = CString::new(src_device)?;
    let mut src_sb: c::bch_sb_handle = unsafe { std::mem::zeroed() };
    let ret = unsafe { c::bch2_read_super(c_path.as_ptr(), &mut opts, &mut src_sb) };
    if ret != 0 {
        let err_str = unsafe { std::ffi::CStr::from_ptr(c::bch2_err_str(ret)).to_string_lossy() };
        return Err(anyhow!("Error opening {}: {}", src_device, err_str));
    }

    // Check member exists
    let m = unsafe { c::bch2_sb_member_get(src_sb.sb, dev_idx) };
    // A member with all-zero UUID is not alive
    if m.uuid.b == [0u8; 16] {
        unsafe { c::bch2_free_super(&mut src_sb) };
        return Err(anyhow!("Member {} does not exist in source superblock", dev_idx));
    }

    // Delete journal fields (they're per-device)
    unsafe {
        c::bch2_sb_field_delete(&mut src_sb, c::bch_sb_field_type::BCH_SB_FIELD_journal);
        c::bch2_sb_field_delete(&mut src_sb, c::bch_sb_field_type::BCH_SB_FIELD_journal_v2);
        (*src_sb.sb).dev_idx = dev_idx as u8;
    }

    // Take ownership of the sb pointer
    let sb = src_sb.sb;
    src_sb.sb = std::ptr::null_mut();
    unsafe { c::bch2_free_super(&mut src_sb) };

    // Set up layout for this device
    let block_size = unsafe { u16::from_le((*sb).block_size) as u32 };
    let bucket_size = u16::from_le(m.bucket_size) as u32;
    let sb_max_size = unsafe { 1u32 << (*sb).layout.sb_max_size_bits };

    unsafe {
        c::bch2_sb_layout_init(
            &mut (*sb).layout,
            block_size << 9,
            bucket_size << 9,
            sb_max_size,
            c::BCH_SB_SECTOR as u64,
            dev_size >> 9,
            false,
        );
    }

    Ok(sb)
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
            let v = parse_size(s)?;
            if v & 511 != 0 {
                return Err(anyhow!("offset must be a multiple of 512"));
            }
            v
        }
        None => 0,
    };

    let scan_len = match &cli.scan_len {
        Some(s) => parse_size(s)?,
        None => 16 << 20, // 16 MiB
    };

    let c_path = CString::new(cli.device.as_str())?;
    let dev_fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDWR) };
    if dev_fd < 0 {
        return Err(anyhow!("{}: {}", cli.device, std::io::Error::last_os_error()));
    }

    let dev_size = match &cli.dev_size {
        Some(s) => parse_size(s)?,
        None => unsafe { c::get_size(dev_fd) },
    };

    let sb = if let Some(ref src) = cli.src_device {
        recover_from_member(src, cli.dev_idx.unwrap(), dev_size)?
    } else {
        recover_from_scan(dev_fd, dev_size, offset, scan_len, cli.verbose)
    };

    // Display the recovered superblock
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
        unsafe { c::bch2_super_write(dev_fd, sb) };
    }

    // Trigger udev to update device database
    let _ = std::process::Command::new("udevadm")
        .args(["trigger", "--settle", &cli.device])
        .status();

    if cli.src_device.is_some() {
        println!("Recovered device will no longer have a journal, please run fsck");
    }

    unsafe {
        libc::free(sb as *mut libc::c_void);
        libc::close(dev_fd);
    }

    Ok(())
}
