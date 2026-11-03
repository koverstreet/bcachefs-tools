//! bcachefs fs du: the space a set of paths references, and how much of the
//! devices it occupies, read from the mounted filesystem's extents and reflink
//! btrees through BCH_IOCTL_QUERY_BTREE_KEYS, which is admin only.
//!
//! Disk is counted by physical region, never by key, because keys share
//! regions: snapshots see the same keys at ancestor snapshot IDs, reflink_p
//! keys share the indirect extent, and every fragment of a split checksummed
//! or compressed extent points at the whole original region, [ptr.offset,
//! +compressed_size). Every dirty pointer's region goes into a per-device
//! interval set, and disk is the length of the union: each replica counts,
//! cached pointers don't, erasure coding parity isn't attributed.
//!
//! A file is identified by (subvolume, inode number): a snapshot keeps its
//! origin's inode numbers, and bcachefs reports the same st_dev for every
//! subvolume, so (st_dev, st_ino) would merge a file with its snapshots. st_dev
//! still tells filesystems apart, and that is the one boundary the walk keeps:
//! it crosses subvolumes and snapshots, where the sharing is, and never enters
//! another filesystem mounted below.
//!
//! Only what's in the btree is counted: dirty pagecache isn't, and inline data
//! and reservations are referenced but take no device space.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use bcachefs_kernel::btree::bkey::prelude::*;
use bcachefs_kernel::btree::bkey::{pos, spos, BkeySC};
use bcachefs_kernel::c;
use bcachefs_kernel::data::ptrs::{BkeyVPtrs, Ptrs};
use bcachefs_kernel::format::Btree;
use bcachefs_kernel::fs::Fs;
use bcachefs_kernel::util::printbuf::Printbuf;
use bcachefs_kernel::util::Sectors;
use bch_bindgen::c::DegradedAction;
use clap::Parser;

use crate::device_scan::open_mounted_for_decoding;
use crate::util::statx_subvol;
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::online_iter::{OnlineBtreeIter, OnlineIterFlags};

#[derive(Parser, Debug)]
#[command(name = "du", about = "Show space used by files and directories",
    long_about = "Shows, for each path, the space its files reference and the \
device space it occupies, counting each physical extent once: data shared \
between snapshots, reflinked copies and hardlinks is counted once, within a \
path and across all paths in the total. Every replica counts toward disk; \
cached copies and erasure coding parity don't. Directories are walked across \
subvolumes and snapshots, never into another filesystem. Requires \
CAP_SYS_ADMIN.",
    disable_help_flag = true)]
pub struct Cli {
    /// Print help
    #[arg(long = "help", action = clap::ArgAction::Help)]
    _help: (),

    /// Human-readable units
    #[arg(short = 'h', long = "human-readable")]
    human_readable: bool,

    /// Files and directories on a mounted bcachefs
    #[arg(default_value = ".")]
    paths: Vec<PathBuf>,
}

/// A dirty pointer's region of a device, in sectors.
#[derive(Clone, Copy)]
struct Region {
    dev:   u32,
    start: u64,
    len:   u64,
}

/// What one file contributes.
#[derive(Default)]
struct FileUsage {
    referenced: u64,
    regions:    Vec<Region>,
}

#[derive(Default)]
struct Usage {
    referenced: u64,
    /// device -> region start -> longest region seen starting there
    regions:    BTreeMap<u32, BTreeMap<u64, u64>>,
}

impl Usage {
    fn add(&mut self, f: &FileUsage) {
        self.referenced += f.referenced;
        for r in &f.regions {
            let len = self.regions.entry(r.dev).or_default().entry(r.start).or_default();
            *len = (*len).max(r.len);
        }
    }

    fn disk(&self) -> u64 {
        self.regions.values()
            .map(|dev| union_len(dev.iter().map(|(&start, &len)| (start, len))))
            .sum()
    }
}

/// Length of the union of (start, len) intervals, sorted by start.
fn union_len(sorted: impl IntoIterator<Item = (u64, u64)>) -> u64 {
    let (mut total, mut covered) = (0, 0);
    for (start, len) in sorted {
        let end = start + len;
        if end > covered {
            total += end - start.max(covered);
            covered = end;
        }
    }
    total
}

fn overlap(a: &Range<u64>, b: &Range<u64>) -> u64 {
    a.end.min(b.end).saturating_sub(a.start.max(b.start))
}

struct Walk<'a> {
    handle:    &'a BcachefsHandle,
    fs:        &'a Fs,
    /// st_dev of the filesystem being walked
    dev:       u64,
    snapshots: HashMap<u64, u32>,
    /// Keys neither counted nor understood, by type: reported, not dropped.
    skipped:   BTreeMap<String, u64>,
    errors:    u64,
}

impl Walk<'_> {
    fn snapshot(&mut self, subvol: u64) -> Result<u32> {
        if let Some(&snapshot) = self.snapshots.get(&subvol) {
            return Ok(snapshot);
        }

        let p = pos(0, subvol);
        let mut iter = OnlineBtreeIter::with_buf_size(self.handle, Btree::subvolumes, 0, p, p,
                                                      OnlineIterFlags::default(), 4096);
        let snapshot = match iter.next()? {
            Some(k) if k.k.p == p => k.arm::<c::Subvolume>()
                .ok_or_else(|| anyhow!("subvolume {subvol}: key at {} is a {:?}", k.k.p, k.k.key_type()))?
                .v().snapshot.get(),
            _ => bail!("subvolume {subvol} not found"),
        };

        self.snapshots.insert(subvol, snapshot);
        Ok(snapshot)
    }

    fn skip(&mut self, k: BkeySC<'_>) {
        *self.skipped.entry(format!("{:?}", k.k.key_type())).or_default() += 1;
    }

    /// The dirty pointers of an extent or indirect extent, and the part of
    /// @live it covers.
    fn ptrs(&mut self, k: BkeySC<'_>, live: &Range<u64>, out: &mut FileUsage) {
        let Some(p) = k.arm::<Ptrs>() else { return self.skip(k) };

        out.referenced += overlap(&(k.k.start_offset()..k.k.p.offset), live);
        out.regions.extend(p.ptrs_decoded(self.fs)
            .filter(|d| d.p.ptr.cached() == 0)
            .map(|d| Region {
                dev:   d.p.ptr.dev() as u32,
                start: d.p.ptr.offset(),
                len:   d.p.crc.compressed_size as u64,
            }));
    }

    fn file_usage(&mut self, subvol: u64, inum: u64) -> Result<FileUsage> {
        use bcachefs_kernel::format::BkeyType as t;

        let snapshot = self.snapshot(subvol)?;
        let mut out = FileUsage::default();
        let mut reflinked = Vec::new();

        let handle = self.handle;
        let mut iter = OnlineBtreeIter::with_buf_size(handle, Btree::extents, 0,
            spos(inum, 0, snapshot), spos(inum, u64::MAX, snapshot),
            OnlineIterFlags::default(), 1 << 14);
        while let Some(k) = iter.next()? {
            if k.k.p.inode != inum {
                break;
            }
            let live = k.k.start_offset()..k.k.p.offset;

            match k.k.key_type() {
                t::extent => self.ptrs(k, &live, &mut out),
                t::reflink_p => {
                    let idx = k.arm::<c::ReflinkP>().expect("a reflink_p").v().idx();
                    reflinked.push(idx..idx + k.k.size as u64);
                }
                t::inline_data | t::reservation =>
                    out.referenced += k.k.size as u64,
                _ => self.skip(k),
            }
        }

        for live in reflinked {
            let mut iter = OnlineBtreeIter::with_buf_size(handle, Btree::reflink, 0,
                pos(0, live.start), pos(0, live.end), OnlineIterFlags::default(), 1 << 14);
            while let Some(k) = iter.next()? {
                if k.k.start_offset() >= live.end {
                    break;
                }
                match k.k.key_type() {
                    t::reflink_v => self.ptrs(k, &live, &mut out),
                    t::indirect_inline_data =>
                        out.referenced += overlap(&(k.k.start_offset()..k.k.p.offset), &live),
                    _ => self.skip(k),
                }
            }
        }

        Ok(out)
    }

    /// Adds what @root holds to @usage, and what @seen_total hasn't seen yet
    /// to @total.
    fn walk(&mut self, root: &Path, usage: &mut Usage,
            total: &mut Usage, seen_total: &mut HashSet<(u64, u64)>) -> Result<()> {
        let mut seen = HashSet::new();
        let mut stack = vec![root.to_path_buf()];

        while let Some(path) = stack.pop() {
            let stx = match statx_subvol(&path, libc::AT_SYMLINK_NOFOLLOW) {
                Ok(Some(stx)) if libc::makedev(stx.stx_dev_major, stx.stx_dev_minor) == self.dev => stx,
                Ok(_) => {
                    eprintln!("{}: on another filesystem, not entered", path.display());
                    continue;
                }
                Err(e) => {
                    eprintln!("{}: {e}", path.display());
                    self.errors += 1;
                    continue;
                }
            };

            let id = (stx.stx_subvol, stx.stx_ino);
            if !seen.insert(id) {
                continue;
            }

            if u32::from(stx.stx_mode) & libc::S_IFMT == libc::S_IFDIR {
                match std::fs::read_dir(&path) {
                    Ok(dir) => for entry in dir {
                        match entry {
                            Ok(entry) => stack.push(entry.path()),
                            Err(e) => {
                                eprintln!("{}: {e}", path.display());
                                self.errors += 1;
                            }
                        }
                    },
                    Err(e) => {
                        eprintln!("{}: {e}", path.display());
                        self.errors += 1;
                    }
                }
                continue;
            }

            let f = self.file_usage(stx.stx_subvol, stx.stx_ino)
                .with_context(|| format!("{}: reading extents", path.display()))?;
            usage.add(&f);
            if seen_total.insert(id) {
                total.add(&f);
            }
        }

        Ok(())
    }
}

/// The mounted filesystem @path is on, and its st_dev.
fn open_fs_of(path: &Path) -> Result<(BcachefsHandle, u64)> {
    let stx = statx_subvol(path, 0)
        .with_context(|| format!("{}", path.display()))?
        .ok_or_else(|| anyhow!("{}: not on a bcachefs filesystem", path.display()))?;

    // a handle comes from a directory: a regular file is never resolved
    let dir = if u32::from(stx.stx_mode) & libc::S_IFMT == libc::S_IFDIR {
        path
    } else {
        path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."))
    };
    let handle = BcachefsHandle::open_if_mounted(dir)
        .map_err(|e| anyhow!("{}: {e}", dir.display()))?
        .ok_or_else(|| anyhow!("{}: not on a mounted bcachefs", path.display()))?;

    Ok((handle, libc::makedev(stx.stx_dev_major, stx.stx_dev_minor)))
}

fn fs_du(cli: Cli) -> Result<()> {
    let (handle, dev) = open_fs_of(&cli.paths[0])?;
    for path in &cli.paths[1..] {
        if open_fs_of(path)?.0.uuid() != handle.uuid() {
            bail!("{}: on a different filesystem than {}", path.display(), cli.paths[0].display());
        }
    }

    let mut opts = c::bch_opts::default();
    opts.set_degraded(DegradedAction::very);
    opts.set_errors(c::bch_error_actions::BCH_ON_ERROR_continue);
    let fs = open_mounted_for_decoding(&handle, opts)?;

    let mut walk = Walk {
        handle: &handle,
        fs:     &fs,
        dev,
        snapshots: HashMap::new(),
        skipped:   BTreeMap::new(),
        errors:    0,
    };
    let mut total = Usage::default();
    let mut seen_total = HashSet::new();
    let mut usages = Vec::new();
    for path in &cli.paths {
        let mut usage = Usage::default();
        walk.walk(path, &mut usage, &mut total, &mut seen_total)?;
        usages.push((path.display().to_string(), usage));
    }
    if usages.len() > 1 {
        usages.push(("total".to_string(), total));
    }

    let mut out = Printbuf::new();
    out.set_human_readable(cli.human_readable);
    out.aligned(|sub| {
        write!(sub, "referenced\rdisk\r  path\n");
        for (name, usage) in &usages {
            write!(sub, "{}\r{}\r  {name}\n", Sectors(usage.referenced), Sectors(usage.disk()));
        }
    });
    print!("{out}");

    for (ty, nr) in &walk.skipped {
        eprintln!("{nr} keys of type {ty} not counted");
    }
    if walk.errors != 0 {
        bail!("{} paths could not be read", walk.errors);
    }
    Ok(())
}

pub const CMD: super::CmdDef = typed_cmd!("du", "Show space used by files and directories", Cli, fs_du);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn union_len_overlapping_nested_adjacent_disjoint() {
        assert_eq!(union_len([]), 0);
        assert_eq!(union_len([(0, 10), (5, 10)]), 15);
        assert_eq!(union_len([(0, 10), (2, 3)]), 10);
        assert_eq!(union_len([(0, 10), (10, 5)]), 15);
        assert_eq!(union_len([(0, 4), (8, 4)]), 8);
        assert_eq!(union_len([(0, 4), (0, 8), (6, 4)]), 10);
    }

    /// Snapshot fragments and split compressed extents: the same region seen
    /// again, or a shorter one inside it, adds nothing.
    #[test]
    fn usage_counts_a_region_once() {
        let file = |referenced, regions: &[(u32, u64, u64)]| FileUsage {
            referenced,
            regions: regions.iter().map(|&(dev, start, len)| Region { dev, start, len }).collect(),
        };
        let mut u = Usage::default();
        u.add(&file(8, &[(0, 100, 4)]));
        u.add(&file(8, &[(0, 100, 4)]));
        u.add(&file(2, &[(0, 101, 2)]));
        u.add(&file(4, &[(1, 100, 4)]));
        assert_eq!(u.referenced, 22);
        assert_eq!(u.disk(), 8);
    }
}
