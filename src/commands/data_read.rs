// bcachefs data-read: read file data with extended error reporting
//
// O_DIRECT read with detailed error information. Reports checksum,
// IO, decompression, and EC errors via a bitmask plus kernel error
// messages. With --no-poison-check, reads data from poisoned extents.

use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;

use bch_bindgen::c;
use clap::Parser;

use crate::wrappers::ioctl::{ioctl_rw, BCHFS_IOC_PREAD_RAW};

const SECTOR_SIZE: u64 = 512;

#[derive(Parser, Debug)]
#[command(name = "data-read")]
/// Read file data with extended error reporting
///
/// Performs an O_DIRECT read and reports any errors encountered during
/// the read (checksum failures, IO errors, decompression failures, EC
/// reconstruction errors) via a structured error bitmask and detailed
/// kernel error messages.
///
/// With --no-poison-check, reads data from extents that were previously
/// marked as poisoned due to checksum failures. This is useful for data
/// recovery — the data may be corrupt, but it's what's on disk.
///
/// Without any flags, behaves like a normal O_DIRECT read but with
/// better error diagnostics.
pub struct Cli {
    /// File to read
    file: String,

    /// Offset in bytes (must be sector-aligned)
    #[arg(long, default_value = "0")]
    offset: u64,

    /// Length in bytes (must be sector-aligned, default: one page)
    #[arg(long, default_value = "4096")]
    len: u64,

    /// Write raw data to this file instead of hex dump
    #[arg(long)]
    output: Option<String>,

    /// Bypass poison checks (read data even from poisoned extents)
    #[arg(long)]
    no_poison_check: bool,

    /// Hex dump width
    #[arg(long, default_value = "16")]
    width: usize,
}

fn cmd_data_read(cli: Cli) -> anyhow::Result<()> {
    cmd_data_read_inner(&cli)
}

fn cmd_data_read_inner(cli: &Cli) -> anyhow::Result<()> {
    if (cli.offset | cli.len) & (SECTOR_SIZE - 1) != 0 {
        anyhow::bail!("offset and len must be sector-aligned ({} bytes)", SECTOR_SIZE);
    }

    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(&cli.file)?;

    // Allocate aligned buffer
    let layout = std::alloc::Layout::from_size_align(cli.len as usize, SECTOR_SIZE as usize)?;
    let buf = unsafe { std::alloc::alloc_zeroed(layout) };
    if buf.is_null() {
        anyhow::bail!("failed to allocate {} byte aligned buffer", cli.len);
    }

    let mut err_msg_buf = vec![0u8; 4096];

    let mut arg = c::bch_ioctl_pread_raw {
        offset: cli.offset,
        len:    cli.len,
        buf:    buf as u64,
        flags:  if cli.no_poison_check { 1 << 0 } else { 0 },
        errors: 0,
        err: c::bch_ioctl_err_msg {
            msg_ptr: err_msg_buf.as_mut_ptr() as u64,
            msg_len: err_msg_buf.len() as u32,
            pad:     0,
        },
    };

    let ret = ioctl_rw::<BCHFS_IOC_PREAD_RAW>(&file, &mut arg);

    // Show error info
    if arg.errors != 0 {
        let mut errors = Vec::new();
        if arg.errors & (1 << 0) != 0 { errors.push("checksum"); }
        if arg.errors & (1 << 1) != 0 { errors.push("io"); }
        if arg.errors & (1 << 2) != 0 { errors.push("decompression"); }
        if arg.errors & (1 << 3) != 0 { errors.push("ec_reconstruct"); }
        eprintln!("errors: {}", errors.join(", "));
    }

    // Show error message from kernel
    let err_len = err_msg_buf.iter().position(|&b| b == 0).unwrap_or(0);
    if err_len > 0 {
        let msg = String::from_utf8_lossy(&err_msg_buf[..err_len]);
        eprintln!("kernel: {}", msg);
    }

    if let Err(errno) = &ret {
        eprintln!("ioctl returned error: {}", errno);
        // Still dump whatever data we got — that's the point
    }

    let data = unsafe { std::slice::from_raw_parts(buf, cli.len as usize) };

    if let Some(ref path) = cli.output {
        std::fs::write(path, data)?;
        println!("wrote {} bytes to {}", data.len(), path);
    } else {
        // Hex dump
        for (i, chunk) in data.chunks(cli.width).enumerate() {
            let offset = cli.offset + (i * cli.width) as u64;
            print!("{:08x}  ", offset);
            for (j, byte) in chunk.iter().enumerate() {
                print!("{:02x} ", byte);
                if j == 7 { print!(" "); }
            }
            // Pad if short line
            for _ in chunk.len()..cli.width {
                print!("   ");
            }
            print!(" |");
            for byte in chunk {
                let c = if byte.is_ascii_graphic() || *byte == b' ' {
                    *byte as char
                } else {
                    '.'
                };
                print!("{}", c);
            }
            println!("|");
        }
    }

    unsafe { std::alloc::dealloc(buf, layout); }

    if ret.is_err() {
        std::process::exit(1);
    }

    Ok(())
}

pub const CMD: super::CmdDef = typed_cmd!("data-read", "Read data with extended error info", Cli, cmd_data_read);
