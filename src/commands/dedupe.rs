use std::collections::{hash_map::DefaultHasher, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::hash::Hasher;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Parser;

use crate::util::{fmt_bytes_human, parse_human_size};

const FIDEDUPERANGE: libc::c_ulong = 0xc0189436;
const FILE_DEDUPE_RANGE_SAME: i32 = 0;
const FILE_DEDUPE_RANGE_DIFFERS: i32 = 1;

#[repr(C)]
#[derive(Default)]
struct FileDedupeRange {
    src_offset: u64,
    src_length: u64,
    dest_count: u16,
    reserved1: u16,
    reserved2: u32,
}

#[repr(C)]
#[derive(Default)]
struct FileDedupeRangeInfo {
    dest_fd: i64,
    dest_offset: u64,
    bytes_deduped: u64,
    status: i32,
    reserved: u32,
}

#[repr(C)]
#[derive(Default)]
struct FileDedupeRangeOne {
    range: FileDedupeRange,
    info: FileDedupeRangeInfo,
}

const _: [(); 24] = [(); std::mem::size_of::<FileDedupeRange>()];
const _: [(); 32] = [(); std::mem::size_of::<FileDedupeRangeInfo>()];
const _: [(); 56] = [(); std::mem::size_of::<FileDedupeRangeOne>()];
const _: [(); 24] = [(); std::mem::offset_of!(FileDedupeRangeOne, info)];

#[derive(Clone, Debug)]
struct Candidate {
    path: PathBuf,
    dev: u64,
    ino: u64,
    size: u64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct SizeKey {
    dev: u64,
    size: u64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct HashKey {
    dev: u64,
    size: u64,
    hash: u64,
}

#[derive(Default)]
struct Stats {
    scanned: u64,
    candidates: u64,
    hashed: u64,
    duplicate_files: u64,
    bytes_shared: u64,
    bytes_would_share: u64,
    differed: u64,
    skipped: u64,
    errors: u64,
}

fn parse_size_arg(s: &str) -> std::result::Result<u64, String> {
    parse_human_size(s).map_err(|e| e.to_string())
}

#[derive(Parser, Debug)]
#[command(
    about = "Deduplicate identical regular files",
    long_about = "\
Scan one or more paths, find regular files with identical contents on the \
same filesystem, and ask the kernel to share their storage with \
FIDEDUPERANGE. The kernel verifies file contents again before sharing, so the \
userspace hash is only a candidate filter."
)]
struct DedupeCli {
    /// Minimum file size to consider
    #[arg(long, default_value = "4k", value_parser = parse_size_arg)]
    min_size: u64,

    /// Show what would be deduplicated without changing files
    #[arg(long)]
    dry_run: bool,

    /// Print each file pair considered for dedupe
    #[arg(short, long)]
    verbose: bool,

    /// Files or directories to scan
    #[arg(required = true)]
    paths: Vec<PathBuf>,
}

fn add_candidate(
    path: &Path,
    meta: &fs::Metadata,
    seen: &mut HashSet<(u64, u64)>,
    stats: &mut Stats,
    out: &mut Vec<Candidate>,
    min_size: u64,
) {
    stats.scanned += 1;

    if meta.len() < min_size {
        stats.skipped += 1;
        return;
    }

    let key = (meta.dev(), meta.ino());
    if !seen.insert(key) {
        stats.skipped += 1;
        return;
    }

    stats.candidates += 1;
    out.push(Candidate {
        path: path.to_path_buf(),
        dev: meta.dev(),
        ino: meta.ino(),
        size: meta.len(),
    });
}

fn scan_path(
    path: &Path,
    min_size: u64,
    seen: &mut HashSet<(u64, u64)>,
    stats: &mut Stats,
    out: &mut Vec<Candidate>,
) -> Result<()> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let ft = meta.file_type();

    if ft.is_symlink() {
        stats.skipped += 1;
    } else if ft.is_file() {
        add_candidate(path, &meta, seen, stats, out, min_size);
    } else if ft.is_dir() {
        let entries =
            fs::read_dir(path).with_context(|| format!("read directory {}", path.display()))?;
        for entry in entries {
            match entry {
                Ok(entry) => {
                    if let Err(e) = scan_path(&entry.path(), min_size, seen, stats, out) {
                        eprintln!("{}: {e:#}", entry.path().display());
                        stats.errors += 1;
                    }
                }
                Err(e) => {
                    eprintln!("{}: {e}", path.display());
                    stats.errors += 1;
                }
            }
        }
    } else {
        stats.skipped += 1;
    }

    Ok(())
}

fn collect_candidates(
    paths: &[PathBuf],
    min_size: u64,
    stats: &mut Stats,
) -> Result<Vec<Candidate>> {
    let mut seen = HashSet::new();
    let mut files = Vec::new();

    for path in paths {
        scan_path(path, min_size, &mut seen, stats, &mut files)?;
    }

    Ok(files)
}

fn verify_opened_candidate(file: &File, candidate: &Candidate) -> Result<bool> {
    let meta = file
        .metadata()
        .with_context(|| format!("stat {}", candidate.path.display()))?;

    Ok(meta.dev() == candidate.dev && meta.ino() == candidate.ino && meta.len() == candidate.size)
}

fn open_candidate_read(candidate: &Candidate) -> Result<Option<File>> {
    let file = File::open(&candidate.path)
        .with_context(|| format!("open {}", candidate.path.display()))?;

    if verify_opened_candidate(&file, candidate)? {
        Ok(Some(file))
    } else {
        Ok(None)
    }
}

fn open_candidate_write(candidate: &Candidate) -> Result<Option<File>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&candidate.path)
        .with_context(|| format!("open {}", candidate.path.display()))?;

    if verify_opened_candidate(&file, candidate)? {
        Ok(Some(file))
    } else {
        Ok(None)
    }
}

fn hash_file(candidate: &Candidate) -> Result<Option<u64>> {
    let Some(mut file) = open_candidate_read(candidate)? else {
        return Ok(None);
    };
    let mut hasher = DefaultHasher::new();
    let mut buf = [0u8; 128 * 1024];

    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("read {}", candidate.path.display()))?;
        if n == 0 {
            break;
        }
        hasher.write(&buf[..n]);
    }

    Ok(Some(hasher.finish()))
}

fn dedupe_one(src: &File, dst: &File, len: u64) -> Result<FileDedupeRangeInfo> {
    let mut arg = FileDedupeRangeOne {
        range: FileDedupeRange {
            src_length: len,
            dest_count: 1,
            ..Default::default()
        },
        info: FileDedupeRangeInfo {
            dest_fd: dst.as_raw_fd().into(),
            ..Default::default()
        },
    };

    let ret = unsafe {
        libc::ioctl(
            src.as_raw_fd(),
            FIDEDUPERANGE,
            &mut arg as *mut FileDedupeRangeOne,
        )
    };
    if ret < 0 {
        return Err(std::io::Error::last_os_error()).context("FIDEDUPERANGE");
    }

    Ok(arg.info)
}

fn dedupe_group(files: &[Candidate], cli: &DedupeCli, stats: &mut Stats) {
    if files.len() < 2 {
        return;
    }

    let mut remaining: Vec<usize> = (0..files.len()).collect();

    while remaining.len() > 1 {
        let src = &files[remaining[0]];
        let src_file = match open_candidate_read(src) {
            Ok(Some(f)) => f,
            Ok(None) => {
                stats.skipped += 1;
                remaining.remove(0);
                continue;
            }
            Err(e) => {
                eprintln!("{e:#}");
                stats.errors += 1;
                remaining.remove(0);
                continue;
            }
        };

        let mut differed = Vec::new();

        for &dst_idx in &remaining[1..] {
            let dst = &files[dst_idx];

            if src.dev == dst.dev && src.ino == dst.ino {
                stats.skipped += 1;
                continue;
            }

            if cli.verbose {
                println!("{} -> {}", src.path.display(), dst.path.display());
            }

            if cli.dry_run {
                stats.duplicate_files += 1;
                stats.bytes_would_share += dst.size;
                continue;
            }

            let dst_file = match open_candidate_write(dst) {
                Ok(Some(f)) => f,
                Ok(None) => {
                    stats.skipped += 1;
                    continue;
                }
                Err(e) => {
                    eprintln!("{e:#}");
                    stats.errors += 1;
                    continue;
                }
            };

            match dedupe_one(&src_file, &dst_file, dst.size) {
                Ok(info) if info.status == FILE_DEDUPE_RANGE_SAME => {
                    stats.duplicate_files += 1;
                    stats.bytes_shared += info.bytes_deduped;
                }
                Ok(info) if info.status == FILE_DEDUPE_RANGE_DIFFERS => {
                    stats.differed += 1;
                    differed.push(dst_idx);
                }
                Ok(info) if info.status < 0 => {
                    let e = std::io::Error::from_raw_os_error(-info.status);
                    eprintln!("{}: {e}", dst.path.display());
                    stats.errors += 1;
                }
                Ok(info) => {
                    eprintln!(
                        "{}: unexpected dedupe status {}",
                        dst.path.display(),
                        info.status
                    );
                    stats.errors += 1;
                }
                Err(e) => {
                    eprintln!("{}: {e:#}", dst.path.display());
                    stats.errors += 1;
                }
            }
        }

        if cli.dry_run {
            break;
        }

        remaining = differed;
    }
}

fn run_pass(cli: &DedupeCli) -> Result<()> {
    let mut stats = Stats::default();
    let files = collect_candidates(&cli.paths, cli.min_size, &mut stats)?;

    let mut by_size: HashMap<SizeKey, Vec<Candidate>> = HashMap::new();
    for file in files {
        by_size
            .entry(SizeKey {
                dev: file.dev,
                size: file.size,
            })
            .or_default()
            .push(file);
    }

    let mut by_hash: HashMap<HashKey, Vec<Candidate>> = HashMap::new();
    for group in by_size.into_values().filter(|g| g.len() > 1) {
        for file in group {
            match hash_file(&file) {
                Ok(Some(hash)) => {
                    stats.hashed += 1;
                    by_hash
                        .entry(HashKey {
                            dev: file.dev,
                            size: file.size,
                            hash,
                        })
                        .or_default()
                        .push(file);
                }
                Ok(None) => {
                    stats.skipped += 1;
                }
                Err(e) => {
                    eprintln!("{e:#}");
                    stats.errors += 1;
                }
            }
        }
    }

    for group in by_hash.into_values().filter(|g| g.len() > 1) {
        dedupe_group(&group, &cli, &mut stats);
    }

    println!("scanned files: {}", stats.scanned);
    println!("candidate files: {}", stats.candidates);
    println!("hashed files: {}", stats.hashed);
    println!("deduped files: {}", stats.duplicate_files);
    if cli.dry_run {
        println!("would share: {}", fmt_bytes_human(stats.bytes_would_share));
    } else {
        println!("shared: {}", fmt_bytes_human(stats.bytes_shared));
    }
    if stats.differed != 0 {
        println!("changed or differed during ioctl: {}", stats.differed);
    }
    if stats.skipped != 0 {
        println!("skipped: {}", stats.skipped);
    }
    if stats.errors != 0 {
        println!("errors: {}", stats.errors);
        bail!("dedupe completed with {} errors", stats.errors);
    }

    Ok(())
}

fn run(cli: DedupeCli) -> Result<()> {
    run_pass(&cli)
}

pub const CMD: super::CmdDef = typed_cmd!(
    "dedupe",
    "Deduplicate identical regular files",
    DedupeCli,
    run
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fideduperange_abi_matches_linux_uapi() {
        assert_eq!(FIDEDUPERANGE, 0xc0189436);
        assert_eq!(std::mem::size_of::<FileDedupeRange>(), 24);
        assert_eq!(std::mem::size_of::<FileDedupeRangeInfo>(), 32);
        assert_eq!(std::mem::size_of::<FileDedupeRangeOne>(), 56);
        assert_eq!(std::mem::offset_of!(FileDedupeRangeOne, info), 24);
        assert_eq!(std::mem::align_of::<FileDedupeRangeOne>(), 8);
    }
}
