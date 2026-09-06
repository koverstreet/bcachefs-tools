// bcachefs data-read: read file data with extended error reporting
//
// O_DIRECT read with detailed error information. Reports checksum,
// IO, decompression, and EC errors via a bitmask plus kernel error
// messages. With --no-poison-check, reads data from poisoned extents.
//
// The read is issued in chunks rather than as one call. Two reasons, both
// load-bearing: the ioctl reports how much it read in its `int` return, so a
// single call can't carry more than i32::MAX; and this is a recovery tool
// pointed at whole files, which are routinely larger than a buffer we want to
// allocate. Chunking bounds the memory and lets the output stream.
//
// Nothing here ever emits a byte it did not read. The buffer is allocated
// zeroed, so padding is indistinguishable from recovered zeros — for a tool
// whose whole job is telling you what is actually on disk, that distinction is
// the product.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;

use bch_bindgen::c;
use clap::Parser;

use crate::wrappers::ioctl::{ioctl_rw, BCHFS_IOC_PREAD_RAW};

const SECTOR_SIZE: u64 = 512;

/// Per-call read size. Bounded by what the ioctl's `int` return can report,
/// and well under that so a multi-gigabyte read needs no such buffer.
const CHUNK: u64 = 64 << 20;

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

/// An O_DIRECT-aligned buffer. Owns its allocation so the read loop can use
/// `?` without leaking it.
struct AlignedBuf {
    ptr:    *mut u8,
    layout: std::alloc::Layout,
}

impl AlignedBuf {
    fn new(size: usize) -> anyhow::Result<Self> {
        let layout = std::alloc::Layout::from_size_align(size, SECTOR_SIZE as usize)?;
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            anyhow::bail!("failed to allocate {} byte aligned buffer", size);
        }
        Ok(Self { ptr, layout })
    }

    fn as_slice(&self, len: usize) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        unsafe { std::alloc::dealloc(self.ptr, self.layout) };
    }
}

/// Where the data goes: a file, or a hex dump on stdout.
enum Sink {
    File(File),
    HexDump { width: usize },
}

impl Sink {
    fn write(&mut self, offset: u64, data: &[u8]) -> anyhow::Result<()> {
        match self {
            Sink::File(f) => Ok(f.write_all(data)?),
            Sink::HexDump { width } => {
                for (i, chunk) in data.chunks(*width).enumerate() {
                    print!("{:08x}  ", offset + (i * *width) as u64);
                    for (j, byte) in chunk.iter().enumerate() {
                        print!("{:02x} ", byte);
                        if j == 7 { print!(" "); }
                    }
                    for _ in chunk.len()..*width {
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
                Ok(())
            }
        }
    }
}

fn errors_to_text(errors: u32) -> String {
    let mut out = Vec::new();
    if errors & (1 << 0) != 0 { out.push("checksum"); }
    if errors & (1 << 1) != 0 { out.push("io"); }
    if errors & (1 << 2) != 0 { out.push("decompression"); }
    if errors & (1 << 3) != 0 { out.push("ec_reconstruct"); }
    out.join(", ")
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

    let buf = AlignedBuf::new(cli.len.min(CHUNK) as usize)?;
    let mut err_msg_buf = vec![0u8; 4096];

    let mut sink = match cli.output {
        Some(ref path) => Sink::File(File::create(path)?),
        None           => Sink::HexDump { width: cli.width },
    };

    let mut pos = 0;
    let mut errors = 0u32;
    let mut failed = false;

    while pos < cli.len {
        let want = (cli.len - pos).min(CHUNK);

        // A message left over from the previous chunk would be read back as
        // this chunk's.
        err_msg_buf.fill(0);

        let mut arg = c::bch_ioctl_pread_raw {
            offset: cli.offset + pos,
            len:    want,
            buf:    buf.ptr as u64,
            flags:  if cli.no_poison_check { 1 << 0 } else { 0 },
            errors: 0,
            err: c::bch_ioctl_err_msg {
                msg_ptr: err_msg_buf.as_mut_ptr() as u64,
                msg_len: err_msg_buf.len() as u32,
                pad:     0,
            },
        };

        let ret = ioctl_rw::<BCHFS_IOC_PREAD_RAW>(&file, &mut arg);
        errors |= arg.errors;

        let err_len = err_msg_buf.iter().position(|&b| b == 0).unwrap_or(0);
        if err_len > 0 {
            eprintln!("kernel: {}", String::from_utf8_lossy(&err_msg_buf[..err_len]));
        }

        let got = match ret {
            Ok(n) => n as u64,
            Err(errno) => {
                eprintln!("read failed at offset {}: {}", cli.offset + pos, errno);
                if pos != 0 {
                    eprintln!("  {} bytes before it were read; resume past the bad \
                               range with --offset", pos);
                }
                failed = true;
                break;
            }
        };

        sink.write(cli.offset + pos, buf.as_slice(got as usize))?;
        pos += got;

        // Short of what was asked for, without an error: end of file.
        if got < want {
            break;
        }
    }

    if !errors_to_text(errors).is_empty() {
        eprintln!("errors: {}", errors_to_text(errors));
    }

    if !failed && pos < cli.len {
        eprintln!("short read: {} of {} bytes requested (end of file?)", pos, cli.len);
    }

    if let Some(ref path) = cli.output {
        println!("wrote {} bytes to {}", pos, path);
    }

    if failed {
        std::process::exit(1);
    }

    Ok(())
}

pub const CMD: super::CmdDef = typed_cmd!("data-read", "Read data with extended error info", Cli, cmd_data_read);
