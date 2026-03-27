// bcachefs unpoison: clear poison flags on file extents
//
// After checksum errors, extents are marked poisoned so subsequent
// reads fail without re-reading from disk. This clears the flag.
//
// WARNING: When poisoned data is moved (rebalance, copygc), it gets
// a new checksum computed over the corrupt data. Unpoisoning means
// the checksum will now PASS — the corruption becomes invisible.
// Only use this if you have independently verified the data is correct
// (e.g. restored from backup) or if you accept the data as-is.

use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;

use clap::Parser;

const SECTOR_SIZE: u64 = 512;

/// ioctl number: _IOW(0xbc, 68, struct bch_ioctl_unpoison)
const fn bchfs_ioc_unpoison() -> libc::c_ulong {
    let size = std::mem::size_of::<BchIoctlUnpoison>() as u32;
    ((1u32 << 30) | (size << 16) | (0xbcu32 << 8) | 68) as libc::c_ulong
}

#[repr(C)]
#[derive(Default)]
struct BchIoctlUnpoison {
    offset:     u64,
    len:        u64,
    flags:      u32,
    pad:        u32,
}

#[derive(Parser, Debug)]
#[command(name = "unpoison")]
/// Clear poison flags on file extents
///
/// WARNING: Poisoned extents contain data that failed checksum verification.
/// When this data is moved (by rebalance, copygc, etc.), the filesystem
/// computes a new checksum over the corrupt data. After unpoisoning, the
/// checksum will PASS and the corruption becomes INVISIBLE.
///
/// Only use this if:
///   - You have restored the file's data from a known-good backup
///   - You have verified the data with `bcachefs data-read` and accept it
///   - You understand that normal reads will return this data without errors
pub struct Cli {
    /// File to unpoison
    file: String,

    /// Start offset in bytes (must be sector-aligned)
    #[arg(long, default_value = "0")]
    offset: u64,

    /// Length in bytes (must be sector-aligned, 0 = entire file)
    #[arg(long, default_value = "0")]
    len: u64,

    /// Required to confirm you understand the risks
    #[arg(long)]
    yes_i_understand: bool,
}

fn cmd_unpoison(cli: Cli) -> anyhow::Result<()> {

    if !cli.yes_i_understand {
        eprintln!("WARNING: Unpoisoning makes corruption invisible.");
        eprintln!();
        eprintln!("Poisoned extents failed checksum verification. After data moves,");
        eprintln!("the corrupt data gets a valid checksum. Unpoisoning removes the");
        eprintln!("only remaining indication that the data is bad.");
        eprintln!();
        eprintln!("Use `bcachefs data-read --no-poison-check` to inspect the data first.");
        eprintln!();
        eprintln!("Pass --yes-i-understand to proceed.");
        std::process::exit(1);
    }

    let mut len = cli.len;
    if len == 0 {
        let meta = std::fs::metadata(&cli.file)?;
        len = (meta.len() + SECTOR_SIZE - 1) & !(SECTOR_SIZE - 1);
    }

    if (cli.offset | len) & (SECTOR_SIZE - 1) != 0 {
        anyhow::bail!("offset and len must be sector-aligned ({} bytes)", SECTOR_SIZE);
    }

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&cli.file)?;

    let arg = BchIoctlUnpoison {
        offset: cli.offset,
        len,
        flags:  0,
        pad:    0,
    };

    let ret = unsafe {
        libc::ioctl(file.as_raw_fd(), bchfs_ioc_unpoison(), &arg as *const _)
    };

    if ret < 0 {
        let errno = std::io::Error::last_os_error();
        anyhow::bail!("ioctl failed: {}", errno);
    }

    println!("unpoisoned {} bytes at offset {}", len, cli.offset);
    Ok(())
}

pub const CMD: super::CmdDef = typed_cmd!("unpoison", "Clear poison flags on file extents", Cli, cmd_unpoison);
