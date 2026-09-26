use std::collections::{hash_map::DefaultHasher, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::hash::Hasher;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

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
    mtime_sec: i64,
    mtime_nsec: i64,
    ctime_sec: i64,
    ctime_nsec: i64,
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

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct InodeKey {
    dev: u64,
    ino: u64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct FileVersion {
    dev: u64,
    ino: u64,
    size: u64,
    mtime_sec: i64,
    mtime_nsec: i64,
    ctime_sec: i64,
    ctime_nsec: i64,
}

impl FileVersion {
    fn inode_key(&self) -> InodeKey {
        InodeKey {
            dev: self.dev,
            ino: self.ino,
        }
    }
}

impl From<&Candidate> for FileVersion {
    fn from(candidate: &Candidate) -> Self {
        Self {
            dev: candidate.dev,
            ino: candidate.ino,
            size: candidate.size,
            mtime_sec: candidate.mtime_sec,
            mtime_nsec: candidate.mtime_nsec,
            ctime_sec: candidate.ctime_sec,
            ctime_nsec: candidate.ctime_nsec,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct DedupePairKey {
    a: FileVersion,
    b: FileVersion,
}

impl DedupePairKey {
    fn new(a: &Candidate, b: &Candidate) -> Self {
        let a = FileVersion::from(a);
        let b = FileVersion::from(b);

        if (a.dev, a.ino) <= (b.dev, b.ino) {
            Self { a, b }
        } else {
            Self { a: b, b: a }
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct CachedHash {
    size: u64,
    mtime_sec: i64,
    mtime_nsec: i64,
    ctime_sec: i64,
    ctime_nsec: i64,
    hash: u64,
}

#[derive(Default)]
struct Stats {
    scanned: u64,
    candidates: u64,
    hashed: u64,
    hash_cache_hits: u64,
    dedupe_cache_hits: u64,
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

    /// Number of scan passes to run; 0 means forever
    #[arg(long, default_value_t = 1)]
    passes: u64,

    /// Seconds to wait between scan passes
    #[arg(long, default_value_t = 300)]
    interval_seconds: u64,

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
        mtime_sec: meta.mtime(),
        mtime_nsec: meta.mtime_nsec(),
        ctime_sec: meta.ctime(),
        ctime_nsec: meta.ctime_nsec(),
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

fn hash_file(candidate: &Candidate) -> Result<Option<(u64, CachedHash)>> {
    let Some(mut file) = open_candidate_read(candidate)? else {
        return Ok(None);
    };
    let meta = file
        .metadata()
        .with_context(|| format!("stat {}", candidate.path.display()))?;
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

    let hash = hasher.finish();
    Ok(Some((
        hash,
        CachedHash {
            size: meta.len(),
            mtime_sec: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
            ctime_sec: meta.ctime(),
            ctime_nsec: meta.ctime_nsec(),
            hash,
        },
    )))
}

fn cached_hash_matches(cached: &CachedHash, candidate: &Candidate) -> bool {
    candidate.size == cached.size
        && candidate.mtime_sec == cached.mtime_sec
        && candidate.mtime_nsec == cached.mtime_nsec
        && candidate.ctime_sec == cached.ctime_sec
        && candidate.ctime_nsec == cached.ctime_nsec
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

fn dedupe_group(
    files: &[Candidate],
    cli: &DedupeCli,
    stats: &mut Stats,
    dedupe_cache: &mut HashSet<DedupePairKey>,
) {
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

            let pair_key = DedupePairKey::new(src, dst);
            if !cli.dry_run && dedupe_cache.contains(&pair_key) {
                stats.dedupe_cache_hits += 1;
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
                    dedupe_cache.insert(pair_key);
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

fn run_pass(
    cli: &DedupeCli,
    hash_cache: &mut HashMap<InodeKey, CachedHash>,
    dedupe_cache: &mut HashSet<DedupePairKey>,
) -> Result<()> {
    let mut stats = Stats::default();
    let files = collect_candidates(&cli.paths, cli.min_size, &mut stats)?;
    let current_inodes: HashSet<InodeKey> = files
        .iter()
        .map(|file| InodeKey {
            dev: file.dev,
            ino: file.ino,
        })
        .collect();

    hash_cache.retain(|inode, _| current_inodes.contains(inode));
    dedupe_cache.retain(|pair| {
        current_inodes.contains(&pair.a.inode_key()) && current_inodes.contains(&pair.b.inode_key())
    });

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
            let inode_key = InodeKey {
                dev: file.dev,
                ino: file.ino,
            };

            let cached_hash = match hash_cache.get(&inode_key) {
                Some(cached) if cached_hash_matches(cached, &file) => {
                    stats.hash_cache_hits += 1;
                    Some(cached.hash)
                }
                _ => None,
            };

            match cached_hash
                .map(|hash| Ok(Some((hash, None))))
                .unwrap_or_else(|| {
                    hash_file(&file).map(|result| result.map(|(hash, cached)| (hash, Some(cached))))
                }) {
                Ok(Some((hash, new_cache))) => {
                    if let Some(cached) = new_cache {
                        stats.hashed += 1;
                        hash_cache.insert(inode_key, cached);
                    }

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
        dedupe_group(&group, &cli, &mut stats, dedupe_cache);
    }

    println!("scanned files: {}", stats.scanned);
    println!("candidate files: {}", stats.candidates);
    println!("hashed files: {}", stats.hashed);
    if stats.hash_cache_hits != 0 {
        println!("hash cache hits: {}", stats.hash_cache_hits);
    }
    if stats.dedupe_cache_hits != 0 {
        println!("dedupe cache hits: {}", stats.dedupe_cache_hits);
    }
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
    if cli.passes == 0 && cli.interval_seconds == 0 {
        bail!("--passes 0 requires a nonzero --interval-seconds");
    }

    let mut pass = 0;
    let mut hash_cache = HashMap::new();
    let mut dedupe_cache = HashSet::new();

    loop {
        pass += 1;

        if cli.passes != 1 {
            if cli.passes == 0 {
                println!("dedupe pass: {pass}");
            } else {
                println!("dedupe pass: {pass}/{}", cli.passes);
            }
        }

        run_pass(&cli, &mut hash_cache, &mut dedupe_cache)?;

        if cli.passes != 0 && pass >= cli.passes {
            break;
        }

        if cli.interval_seconds != 0 {
            thread::sleep(Duration::from_secs(cli.interval_seconds));
        }
    }

    Ok(())
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

    #[test]
    fn cached_hash_requires_unchanged_metadata() {
        let cached = CachedHash {
            size: 4096,
            mtime_sec: 1,
            mtime_nsec: 2,
            ctime_sec: 3,
            ctime_nsec: 4,
            hash: 5,
        };
        let mut candidate = Candidate {
            path: PathBuf::from("file"),
            dev: 10,
            ino: 11,
            size: 4096,
            mtime_sec: 1,
            mtime_nsec: 2,
            ctime_sec: 3,
            ctime_nsec: 4,
        };

        assert!(cached_hash_matches(&cached, &candidate));

        candidate.ctime_nsec = 5;
        assert!(!cached_hash_matches(&cached, &candidate));
    }

    #[test]
    fn dedupe_pair_key_is_order_independent() {
        let a = Candidate {
            path: PathBuf::from("a"),
            dev: 10,
            ino: 11,
            size: 4096,
            mtime_sec: 1,
            mtime_nsec: 2,
            ctime_sec: 3,
            ctime_nsec: 4,
        };
        let b = Candidate {
            path: PathBuf::from("b"),
            dev: 10,
            ino: 12,
            size: 4096,
            mtime_sec: 5,
            mtime_nsec: 6,
            ctime_sec: 7,
            ctime_nsec: 8,
        };

        assert_eq!(DedupePairKey::new(&a, &b), DedupePairKey::new(&b, &a));
    }
}
