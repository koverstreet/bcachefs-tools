//! The space a set of paths references, and how much of the devices it
//! occupies, read file by file from the extents and reflink btrees.
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
//! --compression breaks disk, uncompressed and referenced down by compression
//! type. Uncompressed is per region, like disk, so disk / uncompressed is the
//! compression ratio whatever the replication; referenced goes to the type of
//! the key's first dirty pointer.
//!
//! Only what's in the btree is counted: dirty pagecache isn't, and inline data
//! and reservations are referenced but take no device space.

use std::collections::{BTreeMap, HashSet};
use std::ops::Range;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use bcachefs_kernel::btree::bkey::prelude::*;
use bcachefs_kernel::btree::bkey::{pos, spos, BkeySC};
use bcachefs_kernel::data::ptrs::{BkeyVPtrs, Ptrs};
use bcachefs_kernel::format::{Btree, CompressionType};
use bcachefs_kernel::fs::Fs;
use bcachefs_kernel::util::printbuf::Printbuf;
use bcachefs_kernel::util::{Sectors, ToText};

use super::{reflink_p_range, Cli, Mounted, Subvols};
use crate::util::statx_subvol;
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::online_iter::{OnlineBtreeIter, OnlineIterFlags};

/// A dirty pointer's region of a device, in sectors.
#[derive(Clone, Copy)]
struct Region {
    dev:          u32,
    start:        u64,
    len:          u64,
    uncompressed: u64,
    ctype:        u8,
}

/// What one file contributes.
#[derive(Default)]
struct FileUsage {
    referenced:         u64,
    referenced_by_type: BTreeMap<u8, u64>,
    regions:            Vec<Region>,
}

#[derive(Default)]
struct Usage {
    referenced:         u64,
    referenced_by_type: BTreeMap<u8, u64>,
    /// (compression type, device) -> region start -> (longest region seen
    /// starting there, its uncompressed size)
    regions:            BTreeMap<(u8, u32), BTreeMap<u64, (u64, u64)>>,
}

/// One row of the --compression table, in sectors.
struct TypeUsage {
    ctype:        u8,
    disk:         u64,
    uncompressed: u64,
    referenced:   u64,
}

impl Usage {
    fn add(&mut self, f: &FileUsage) {
        self.referenced += f.referenced;
        for (&ctype, &sectors) in &f.referenced_by_type {
            *self.referenced_by_type.entry(ctype).or_default() += sectors;
        }
        for r in &f.regions {
            let e = self.regions.entry((r.ctype, r.dev)).or_default().entry(r.start).or_default();
            *e = (e.0.max(r.len), r.uncompressed);
        }
    }

    fn disk(&self) -> u64 {
        let mut by_dev: BTreeMap<u32, Vec<(u64, u64)>> = BTreeMap::new();
        for (&(_, dev), regions) in &self.regions {
            by_dev.entry(dev).or_default().extend(regions.iter().map(|(&start, &(len, _))| (start, len)));
        }
        by_dev.into_values()
            .map(|mut regions| {
                regions.sort_unstable();
                union_len(regions)
            })
            .sum()
    }

    fn by_type(&self) -> Vec<TypeUsage> {
        fn row(rows: &mut BTreeMap<u8, TypeUsage>, ctype: u8) -> &mut TypeUsage {
            rows.entry(ctype).or_insert(TypeUsage { ctype, disk: 0, uncompressed: 0, referenced: 0 })
        }
        let mut rows = BTreeMap::new();

        for (&(ctype, _), regions) in &self.regions {
            let disk = union_len(regions.iter().map(|(&start, &(len, _))| (start, len)));
            let compressed = ctype as u32 != CompressionType::none.0 &&
                ctype as u32 != CompressionType::incompressible.0;

            let r = row(&mut rows, ctype);
            r.disk += disk;
            // a compressed region is decompressed whole; an uncompressed one
            // may be a fragment overlapping another
            r.uncompressed += if compressed {
                regions.values().map(|&(_, uncompressed)| uncompressed).sum()
            } else {
                disk
            };
        }
        for (&ctype, &sectors) in &self.referenced_by_type {
            row(&mut rows, ctype).referenced += sectors;
        }
        rows.into_values().collect()
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
    subvols:   Subvols,
    /// Keys neither counted nor understood, by type: reported, not dropped.
    skipped:   BTreeMap<String, u64>,
    errors:    u64,
}

impl Walk<'_> {
    fn snapshot(&mut self, subvol: u64) -> Result<u32> {
        self.subvols.snapshot(self.handle, subvol)?
            .ok_or_else(|| anyhow!("subvolume {subvol} not found"))
    }

    fn skip(&mut self, k: BkeySC<'_>) {
        *self.skipped.entry(format!("{:?}", k.k.key_type())).or_default() += 1;
    }

    /// The dirty pointers of an extent or indirect extent, and the part of
    /// @live it covers.
    fn ptrs(&mut self, k: BkeySC<'_>, live: &Range<u64>, out: &mut FileUsage) {
        let Some(p) = k.arm::<Ptrs>() else { return self.skip(k) };

        let first = out.regions.len();
        out.regions.extend(p.ptrs_decoded(self.fs)
            .filter(|d| d.p.ptr.cached() == 0)
            .map(|d| Region {
                dev:          d.p.ptr.dev() as u32,
                start:        d.p.ptr.offset(),
                len:          d.p.crc.compressed_size as u64,
                uncompressed: d.p.crc.uncompressed_size as u64,
                ctype:        d.p.crc.compression_type,
            }));

        let referenced = overlap(&(k.k.start_offset()..k.k.p.offset), live);
        out.referenced += referenced;
        if let Some(r) = out.regions.get(first) {
            *out.referenced_by_type.entry(r.ctype).or_default() += referenced;
        }
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
                t::reflink_p => reflinked.extend(reflink_p_range(k)),
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

pub(super) fn run(m: &Mounted, cli: &Cli) -> Result<()> {
    let mut walk = Walk {
        handle:  &m.handle,
        fs:      &m.fs,
        dev:     m.dev,
        subvols: Subvols::default(),
        skipped: BTreeMap::new(),
        errors:  0,
    };
    let mut total = Usage::default();
    let mut seen_total = HashSet::new();
    let mut usages = Vec::new();
    for path in &cli.paths {
        let mut usage = Usage::default();
        walk.walk(path, &mut usage, &mut total, &mut seen_total)?;
        usages.push((path.display().to_string(), usage));
    }
    let types = cli.compression.then(|| total.by_type());
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
    if let Some(types) = types {
        out.aligned(|sub| {
            write!(sub, "type\tdisk\runcompressed\rreferenced\rratio\r\n");
            for t in &types {
                CompressionType(t.ctype as u32).to_text(sub);
                write!(sub, "\t{}\r{}\r{}\r", Sectors(t.disk), Sectors(t.uncompressed), Sectors(t.referenced));
                match (t.disk * 1000).checked_div(t.uncompressed) {
                    Some(pm) => write!(sub, "{}.{}%\r\n", pm / 10, pm % 10),
                    None => write!(sub, "-\r\n"),
                }
            }
        });
    }
    print!("{out}");

    for (ty, nr) in &walk.skipped {
        eprintln!("{nr} keys of type {ty} not counted");
    }
    if walk.errors != 0 {
        bail!("{} paths could not be read", walk.errors);
    }
    Ok(())
}

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
            regions: regions.iter()
                .map(|&(dev, start, len)| Region { dev, start, len, uncompressed: len, ctype: 0 })
                .collect(),
            ..Default::default()
        };
        let mut u = Usage::default();
        u.add(&file(8, &[(0, 100, 4)]));
        u.add(&file(8, &[(0, 100, 4)]));
        u.add(&file(2, &[(0, 101, 2)]));
        u.add(&file(4, &[(1, 100, 4)]));
        assert_eq!(u.referenced, 22);
        assert_eq!(u.disk(), 8);
    }

    /// A zstd extent split by an overwrite, on two replicas, and an
    /// uncompressed fragment: each compressed region decompresses once per
    /// replica, so the ratio is the compression ratio.
    #[test]
    fn by_type_counts_compressed_regions_once() {
        let zstd = CompressionType::zstd.0 as u8;
        let region = |dev, start, len, uncompressed, ctype| Region { dev, start, len, uncompressed, ctype };
        let fragment = |referenced| FileUsage {
            referenced,
            referenced_by_type: [(zstd, referenced)].into(),
            regions: vec![region(0, 100, 8, 32, zstd), region(1, 500, 8, 32, zstd)],
        };
        let mut u = Usage::default();
        u.add(&fragment(10));
        u.add(&fragment(12));
        u.add(&FileUsage {
            referenced: 4,
            referenced_by_type: [(0, 4)].into(),
            regions: vec![region(0, 200, 4, 4, 0)],
        });

        let rows = u.by_type();
        let row = |ctype| rows.iter().find(|r| r.ctype == ctype).expect("a row");
        assert_eq!((row(zstd).disk, row(zstd).uncompressed, row(zstd).referenced), (16, 64, 22));
        assert_eq!((row(0).disk, row(0).uncompressed, row(0).referenced), (4, 4, 4));
        assert_eq!(u.disk(), 20);
    }
}
