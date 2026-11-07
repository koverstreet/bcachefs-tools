//! fs du --sample: what the devices hold, estimated from random sectors
//! instead of read from every file's extents - the btdu estimator. The cost is
//! the number of samples, whatever the size of the filesystem.
//!
//! Every member device's sectors, [0, nbuckets * bucket_size), are laid end to
//! end and each sample is a uniformly random sector of that space, so one
//! stands for capacity / samples of it, and an owner's share of samples
//! estimates its share of the devices. The error of a share p of n samples is
//! binomial, sqrt(p (1 - p) / n): the ±95% column is 1.96 times that.
//!
//! A sector is mapped to what holds it without any ioctl of its own: one
//! BCH_IOCTL_QUERY_BTREE_KEYS on the backpointers btree, backwards from the
//! sector's last backpointer position to its bucket's first - an extent never
//! crosses a bucket - finds the greatest backpointer at or below it. Regions on
//! a device don't overlap, so only the backpointers at that one sector can
//! cover it: the fragments of one encoded extent, which share its pointer's
//! sector at different crc offsets. Their bucket_len is the fragment's, not
//! the region's, so coverage comes from the key each points to, looked up
//! exactly at its pos and checked to point back as backpointer_get_key()
//! checks: a sector is the key's if it's in [ptr.offset, +compressed_size),
//! the region exact mode counts as disk. Among fragments, the one whose live
//! data is at the sector wins.
//!
//! Superblock and journal sectors have no backpointers, and are known from
//! the superblock. Erasure coding parity has its backpointer at the end of
//! its block, past the sector; when nothing below covers a sector and the
//! filesystem has stripes, a second query looks forward for one. Sectors no
//! backpointer covers are unused: free buckets, and the unwritten or
//! overwritten parts of written ones. Btree nodes are counted by their
//! backpointer alone.
//!
//! A sample of dirty data belongs to the inode and snapshot of its key, or to
//! the range of the reflink btree its indirect extent covers, shared equally
//! by the files referencing it, as reflink.rs finds them. Each distinct file
//! is resolved to a path once, after sampling, by resolve.rs. A path's
//! estimate is then the samples of the files below it, and every sector is
//! one path's: data shared between snapshots goes to the snapshot its key is
//! in, where exact mode counts it in each path that sees it. Cached copies
//! go to no path, as in exact mode.
//!
//! The data type table, whole and with --devices per device, puts the kernel's
//! accounting from BCH_IOCTL_DEV_USAGE beside each estimate. They count a
//! region partly overwritten differently: accounting its live sectors, the
//! sample the whole region, which stays allocated until it's rewritten. Its
//! unused is what accounting leaves of the capacity. --targets groups file
//! data by the background target of the file that owns it.
//!
//! Backpointers go through the btree write buffer, and the ioctl doesn't see
//! what's still in it: data written since the last write buffer flush shows
//! as unused. A backpointer whose key doesn't point back is counted as stale,
//! not dropped. Samples are drawn by --threads threads on the one handle, the
//! ioctl being stateless; --seed makes a run repeatable for a given --threads.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::ops::Range;
use std::path::{Component, Path};
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use bcachefs_kernel::alloc::backpointers::{backpointers_match, extent_ptr_to_bp};
use bcachefs_kernel::alloc::background::alloc_to_v4;
use bcachefs_kernel::alloc::buckets::gen_after;
use bcachefs_kernel::alloc::disk_groups::Target;
use bcachefs_kernel::btree::bkey::prelude::*;
use bcachefs_kernel::btree::bkey::{pos, BkeySC, POS_MAX, POS_MIN};
use bcachefs_kernel::c;
use bcachefs_kernel::data::ptrs::{crc_is_compressed, BkeyVPtrs, Ptrs};
use bcachefs_kernel::format::{Bpos, Btree, DataType};
use bcachefs_kernel::fs::Fs;
use bcachefs_kernel::util::printbuf::Printbuf;
use bcachefs_kernel::util::rcu::read_lock;
use bcachefs_kernel::util::{Sectors, ToText};

use super::reflink::ReflinkRefs;
use super::resolve::{fs_path, Resolver};
use super::{Cli, Mounted};
use crate::wrappers::handle::{BcachefsHandle, DevUsage};
use crate::wrappers::online_iter::{OnlineBtreeIter, OnlineIterFlags};

/// One member device's part of the sampled space.
struct DevSpace {
    idx:         u32,
    name:        String,
    /// Where it starts in the space
    start:       u64,
    bucket_size: u64,
    /// Superblock copies, and what precedes the first
    sb:          Vec<Range<u64>>,
    journal:     HashSet<u64>,
}

/// The member devices end to end.
struct Space {
    devs:  Vec<DevSpace>,
    total: u64,
}

impl Space {
    fn new(fs: &Fs) -> Result<Self> {
        let rcu = read_lock();
        let mut devs = Vec::new();
        let mut total = 0;

        for ca in fs.devs_rcu(&rcu) {
            let mi = ca.mi.get();
            let bucket_size = u64::from(mi.bucket_size);
            let sectors = mi.nbuckets * bucket_size;

            let l = ca.sb_layout.get();
            let sb_sectors = 1u64 << l.sb_max_size_bits;
            let mut sb: Vec<_> = l.sb_offset[..usize::from(l.nr_superblocks)].iter()
                .map(|o| o.get()..o.get() + sb_sectors)
                .collect();
            if sb.first().is_some_and(|r| r.start == u64::from(c::BCH_SB_SECTOR)) {
                sb.push(0..u64::from(c::BCH_SB_SECTOR));
            }

            devs.push(DevSpace {
                idx: ca.dev_idx.into(),
                name: ca.to_string(),
                start: total,
                bucket_size,
                sb,
                journal: ca.with_journal_ring(|r| r.buckets().iter().copied().collect()),
            });
            total += sectors;
        }

        if total == 0 {
            bail!("no member device has any buckets");
        }
        Ok(Space { devs, total })
    }

    /// The device and sector @x of the space is.
    fn locate(&self, x: u64) -> (usize, u64) {
        let i = self.devs.partition_point(|d| d.start <= x) - 1;
        (i, x - self.devs[i].start)
    }
}

/// What a sample landed on: a data type, as fs usage counts them, or none.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
enum Kind {
    Data(u32),
    /// No backpointer covers it
    Unused,
    /// A backpointer is there, but what it points to doesn't point back
    Stale,
}

impl Kind {
    fn data(t: DataType) -> Self {
        Kind::Data(t.0)
    }
}

/// Whose data a sample is: the key it's in names an inode in a snapshot, or a
/// range of the reflink btree that files reference.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Owner {
    Inode { inum: u64, snapshot: u32 },
    Indirect(Range<u64>),
}

struct Sample {
    dev:   usize,
    kind:  Kind,
    owner: Option<Owner>,
}

/// A backpointer read at a sector.
#[derive(Clone, Copy)]
struct Bp {
    pos: Bpos,
    v:   c::Backpointer,
}

/// What a backpointer's key holds at the sampled sector.
enum Covers {
    No,
    /// The sector is in the key's region, at live data or not
    Region { live: bool, kind: Kind, owner: Option<Owner> },
    Stale,
}

struct Sampler<'a> {
    handle:      &'a BcachefsHandle,
    fs:          &'a Fs,
    space:       &'a Space,
    shift:       u32,
    has_stripes: bool,
}

impl Sampler<'_> {
    fn bp_pos(&self, dev: u32, sector: u64, suboffset: u64) -> Bpos {
        pos(dev.into(), (sector << self.shift) | suboffset)
    }

    fn bp_sector(&self, p: Bpos) -> u64 {
        p.offset >> self.shift
    }

    fn sample(&self, dev: usize, sector: u64) -> Result<Sample> {
        let d = &self.space.devs[dev];
        let kind = |kind| Ok(Sample { dev, kind, owner: None });

        if d.sb.iter().any(|r| r.contains(&sector)) {
            return kind(Kind::data(DataType::sb));
        }
        let bucket = sector / d.bucket_size;
        if d.journal.contains(&bucket) {
            return kind(Kind::data(DataType::journal));
        }

        // Fragments of one region share its sector and its crc: one that
        // doesn't reach @sector rules them all out.
        let end = self.bp_pos(d.idx, bucket * d.bucket_size, 0);
        let mut p = self.bp_pos(d.idx, sector, (1 << self.shift) - 1);
        let mut at = None;
        let (mut found, mut stale, mut stripe) = (None, false, None);

        while let Some(bp) = self.bp_at_or_below(p, end)? {
            let bp_sector = self.bp_sector(bp.pos);

            if Btree(bp.v.btree_id.into()) == Btree::stripes {
                stripe = stripe.or((bp_sector == sector).then_some(bp));
            } else if self.stale_cached(d.idx, bucket, &bp)? {
                // its region may hold anything since
            } else if *at.get_or_insert(bp_sector) != bp_sector {
                break;
            } else {
                match self.covers(sector, &bp)? {
                    Covers::Region { live: true, kind, owner } => return Ok(Sample { dev, kind, owner }),
                    Covers::Region { live: false, kind, owner } => { found.get_or_insert((kind, owner)); }
                    Covers::Stale => stale = true,
                    Covers::No => break,
                }
            }

            if bp.pos <= end {
                break;
            }
            p = bp.pos.nosnap_predecessor();
        }
        if let Some((kind, owner)) = found {
            return Ok(Sample { dev, kind, owner });
        }

        let stripe = match stripe {
            Some(bp) => Some(bp),
            None if self.has_stripes => self.stripe_bp_after(d.idx, sector, (bucket + 1) * d.bucket_size)?,
            None => None,
        };
        if let Some(bp) = stripe.filter(|bp| self.stripe_block(bp).contains(&sector)) {
            return kind(Kind::Data(bp.v.data_type.into()));
        }

        kind(if stale { Kind::Stale } else { Kind::Unused })
    }

    /// The greatest backpointer at or below @p, down to @end. One per call:
    /// the ioctl's prev walk steps forward after a key, and returns that key
    /// again until its buffer is full.
    fn bp_at_or_below(&self, p: Bpos, end: Bpos) -> Result<Option<Bp>> {
        let mut iter = OnlineBtreeIter::with_buf_size(self.handle, Btree::backpointers, 0, p, end,
                                                      OnlineIterFlags::PREV, 256);
        Ok(iter.next()?
            .and_then(|k| k.arm::<c::Backpointer>().map(|bp| Bp { pos: k.k.p, v: *bp.v() })))
    }

    /// Whether @bp is a cached pointer's whose bucket has been invalidated
    /// since, its generation moved on: no longer data, as ptr_stale() says.
    fn stale_cached(&self, dev: u32, bucket: u64, bp: &Bp) -> Result<bool> {
        if DataType(bp.v.data_type.into()) != DataType::cached {
            return Ok(false);
        }
        let p = pos(dev.into(), bucket);
        let mut iter = OnlineBtreeIter::with_buf_size(self.handle, Btree::alloc, 0, p, p,
                                                      OnlineIterFlags::default(), 256);
        let generation = match iter.next()? {
            Some(k) if k.k.p == p => alloc_to_v4(k).generation,
            _ => 0,
        };
        Ok(gen_after(generation, bp.v.bucket_gen) != 0)
    }

    /// The first stripe backpointer after @sector, before @bucket_end.
    fn stripe_bp_after(&self, dev: u32, sector: u64, bucket_end: u64) -> Result<Option<Bp>> {
        let start = self.bp_pos(dev, sector, 0);
        let end = self.bp_pos(dev, bucket_end, 0).nosnap_predecessor();
        let mut iter = OnlineBtreeIter::with_buf_size(self.handle, Btree::backpointers, 0, start, end,
                                                      OnlineIterFlags::default(), 1 << 10);
        while let Some(k) = iter.next()? {
            if let Some(bp) = k.arm::<c::Backpointer>()
                .filter(|bp| Btree(bp.v().btree_id.into()) == Btree::stripes) {
                return Ok(Some(Bp { pos: k.k.p, v: *bp.v() }));
            }
        }
        Ok(None)
    }

    /// A stripe block's sectors: its backpointer is at the last.
    fn stripe_block(&self, bp: &Bp) -> Range<u64> {
        let last = self.bp_sector(bp.pos);
        last + 1 - u64::from(bp.v.bucket_len)..last + 1
    }

    fn covers(&self, sector: u64, bp: &Bp) -> Result<Covers> {
        let v = bp.v;
        let btree = Btree(v.btree_id.into());
        let start = self.bp_sector(bp.pos);

        if v.level != 0 {
            let live = (start..start + u64::from(v.bucket_len)).contains(&sector);
            return Ok(if live {
                Covers::Region { live, kind: Kind::Data(v.data_type.into()), owner: None }
            } else {
                Covers::No
            });
        }

        let flags = if btree == Btree::extents { OnlineIterFlags::ALL_SNAPSHOTS } else { OnlineIterFlags::default() };
        let mut iter = OnlineBtreeIter::with_buf_size(self.handle, btree, 0, v.pos, v.pos, flags, 1 << 10);
        let Some(k) = iter.next()?.filter(|k| k.k.p == v.pos) else { return Ok(Covers::Stale) };
        let Some(p) = self.points_back(btree, k, bp) else { return Ok(Covers::Stale) };

        let region = p.ptr.offset()..p.ptr.offset() + u64::from(p.crc.compressed_size);
        if !region.contains(&sector) {
            return Ok(Covers::No);
        }
        let live_start = p.ptr.offset() + u64::from(p.crc.offset);
        let live = crc_is_compressed(&p.crc) ||
            (live_start..live_start + u64::from(k.k.size)).contains(&sector);

        let owner = match btree {
            Btree::extents => Some(Owner::Inode { inum: k.k.p.inode, snapshot: k.k.p.snapshot }),
            Btree::reflink => Some(Owner::Indirect(k.k.start_offset()..k.k.p.offset)),
            _ => None,
        };
        Ok(Covers::Region { live, kind: Kind::Data(v.data_type.into()), owner })
    }

    /// @k's pointer @bp is the backpointer of, as backpointer_get_key() matches
    /// them.
    fn points_back(&self, btree: Btree, k: BkeySC<'_>, bp: &Bp) -> Option<c::extent_ptr_decoded> {
        k.arm::<Ptrs>()?.ptrs_decoded(self.fs)
            .find(|d| {
                let want = extent_ptr_to_bp(self.fs, btree, 0, k, d);
                want.k.p == bp.pos && backpointers_match(&bp.v, &want.v)
            })
            .map(|d| d.p)
    }
}

/// Samples counted.
#[derive(Default)]
struct Tally {
    n:      u64,
    /// (device, kind) -> samples
    kinds:  BTreeMap<(usize, Kind), u64>,
    /// Of dirty data: cached copies aren't disk, as exact mode counts it
    owners: HashMap<Owner, u64>,
}

impl Tally {
    fn add(&mut self, s: Sample) {
        self.n += 1;
        *self.kinds.entry((s.dev, s.kind)).or_default() += 1;
        if let Some(owner) = s.owner.filter(|_| s.kind != Kind::data(DataType::cached)) {
            *self.owners.entry(owner).or_default() += 1;
        }
    }

    fn merge(&mut self, other: Tally) {
        self.n += other.n;
        for (k, n) in other.kinds {
            *self.kinds.entry(k).or_default() += n;
        }
        for (k, n) in other.owners {
            *self.owners.entry(k).or_default() += n;
        }
    }
}

fn sample_n(sampler: &Sampler<'_>, n: u64, seed: u64) -> Result<Tally> {
    let mut rng = fastrand::Rng::with_seed(seed);
    let mut tally = Tally::default();

    for _ in 0..n {
        let (dev, sector) = sampler.space.locate(rng.u64(..sampler.space.total));
        let s = sampler.sample(dev, sector)
            .with_context(|| format!("sampling {} sector {sector}", sampler.space.devs[dev].name))?;
        tally.add(s);
    }
    Ok(tally)
}

/// @n samples over @threads threads, thread i seeded with @seed + i.
fn sample(sampler: &Sampler<'_>, n: u64, seed: u64, threads: u64) -> Result<Tally> {
    let threads = threads.clamp(1, n.max(1));

    std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads)
            .map(|i| {
                let n = n / threads + u64::from(i < n % threads);
                s.spawn(move || sample_n(sampler, n, seed.wrapping_add(i)))
            })
            .collect();

        let mut tally = Tally::default();
        for w in workers {
            // a panic aborts: panic = "abort"
            tally.merge(w.join().expect("sampling thread")?);
        }
        Ok(tally)
    })
}

/// An estimate from @samples of @t's: sectors, and its ±95% margin.
fn estimate(t: &Tally, total: u64, samples: f64) -> (u64, u64) {
    let n = t.n as f64;
    let p = samples / n;
    let total = total as f64;
    ((total * p).round() as u64, (1.96 * total * (p * (1.0 - p) / n).sqrt()).round() as u64)
}

fn kind_to_text(out: &mut Printbuf, kind: Kind) {
    match kind {
        Kind::Data(t) => DataType(t).to_text(out),
        Kind::Unused  => write!(out, "unused"),
        Kind::Stale   => write!(out, "stale backpointer"),
    }
}

/// Whether the filesystem has any erasure coded stripes.
fn has_stripes(handle: &BcachefsHandle) -> Result<bool> {
    let mut iter = OnlineBtreeIter::with_buf_size(handle, Btree::stripes, 0, POS_MIN, POS_MAX,
                                                  OnlineIterFlags::default(), 1 << 10);
    Ok(iter.next()?.is_some())
}

pub(super) fn run(m: &Mounted, cli: &Cli) -> Result<()> {
    if cli.samples == 0 {
        bail!("--samples must be at least 1");
    }

    let space = Space::new(&m.fs)?;
    let sampler = Sampler {
        handle:      &m.handle,
        fs:          &m.fs,
        space:       &space,
        shift:       m.fs.sb.extent_bp_shift.get().into(),
        has_stripes: has_stripes(&m.handle).context("looking for stripes")?,
    };
    let seed = cli.seed.unwrap_or_else(|| fastrand::u64(..));
    let threads = cli.threads.unwrap_or_else(|| {
        std::thread::available_parallelism().map_or(1, |n| n.get() as u64).min(8)
    });

    let started = Instant::now();
    let tally = sample(&sampler, cli.samples, seed, threads)?;
    let elapsed = started.elapsed();

    let mut resolver = Resolver::new(&m.handle, &m.fs)?;
    let mut tree = Tree::default();
    let (files, unreferenced) = attribute(&m.handle, &tally)?;
    let mut unresolved = 0.0;
    for (&(inum, snapshot), &n) in &files {
        match resolver.path(inum, snapshot)
            .with_context(|| format!("resolving inode {inum} in snapshot {snapshot}"))? {
            Some(path) => tree.add(&path, n),
            None => unresolved += n,
        }
    }

    let mut roots = Vec::new();
    for path in &cli.paths {
        let fs_path = fs_path(&mut resolver, path).with_context(|| format!("{}", path.display()))?;
        roots.push((path, fs_path));
    }
    let total = tree.union(roots.iter().map(|(_, p)| p.as_path()));

    let usage = space.devs.iter()
        .map(|d| m.handle.dev_usage(d.idx).map_err(|e| anyhow!("{}: reading its usage: {e}", d.name)))
        .collect::<Result<Vec<_>>>()?;
    let all_devs: Vec<usize> = (0..space.devs.len()).collect();

    let mut targets: BTreeMap<String, f64> = BTreeMap::new();
    if cli.targets {
        for (&(inum, snapshot), &n) in &files {
            let label = match resolver.background_target(inum, snapshot)? {
                Some(t) => {
                    let mut label = Printbuf::new();
                    m.fs.target_to_text(&mut label, Target::decode(t.into()));
                    label.to_string()
                }
                None => "(unresolved)".to_string(),
            };
            *targets.entry(label).or_default() += n;
        }
        if unreferenced != 0.0 {
            targets.insert("(unreferenced)".to_string(), unreferenced);
        }
    }

    let mut out = Printbuf::new();
    out.set_human_readable(cli.human_readable);
    writeln!(out, "{} samples of {} on {} devices in {:.1}s, seed {seed}, threads {threads}",
           tally.n, Sectors(space.total), space.devs.len(), elapsed.as_secs_f64());
    out.aligned(|sub| {
        let est = |sub: &mut Printbuf, n: f64| {
            let (est, margin) = estimate(&tally, space.total, n);
            write!(sub, "{}\r{}\r  ", Sectors(est), Sectors(margin));
        };

        write!(sub, "disk\r±95%\r  path\n");
        for (path, fs_path) in &roots {
            let empty = Tree::default();
            let t = tree.get(fs_path).unwrap_or(&empty);
            t.to_text(sub, path, cli.max_depth, &est);
        }
        if roots.len() > 1 {
            est(sub, total);
            writeln!(sub, "total");
        }

        let kinds = |sub: &mut Printbuf, devs: &[usize]| {
            write!(sub, "estimate\r±95%\raccounted\r  type\n");
            for (kind, (n, accounted)) in kind_rows(&tally, devs, &usage) {
                let (est, margin) = estimate(&tally, space.total, n as f64);
                write!(sub, "{}\r{}\r", Sectors(est), Sectors(margin));
                match accounted {
                    Some(sectors) => write!(sub, "{}\r  ", Sectors(sectors)),
                    None          => write!(sub, "-\r  "),
                }
                kind_to_text(sub, kind);
                writeln!(sub);
            }
        };

        writeln!(sub);
        kinds(sub, &all_devs);
        for (n, what) in [(unresolved, "user, unresolved"), (unreferenced, "user, reflinked, unreferenced")] {
            if n != 0.0 {
                est(sub, n);
                writeln!(sub, "{what}");
            }
        }

        if cli.devices {
            for (i, d) in space.devs.iter().enumerate() {
                writeln!(sub, "\n{} (device {}):", d.name, d.idx);
                kinds(sub, &[i]);
            }
        }

        if cli.targets {
            write!(sub, "\nestimate\r±95%\r  background target\n");
            for (label, &n) in &targets {
                est(sub, n);
                writeln!(sub, "{label}");
            }
        }
    });
    print!("{out}");
    Ok(())
}

/// Samples by (inode, snapshot).
type Files = HashMap<(u64, u32), f64>;

/// Samples of file data by (inode, snapshot), a reflinked sample shared
/// equally by the files referencing it; and those of reflinked data nothing
/// references.
fn attribute(handle: &BcachefsHandle, tally: &Tally) -> Result<(Files, f64)> {
    let refs = if tally.owners.keys().any(|o| matches!(o, Owner::Indirect(_))) {
        Some(ReflinkRefs::load(handle).context("reading reflink pointers")?)
    } else {
        None
    };
    let mut files = Files::new();
    let mut unreferenced = 0.0;

    for (owner, &n) in &tally.owners {
        match owner {
            &Owner::Inode { inum, snapshot } => *files.entry((inum, snapshot)).or_default() += n as f64,
            Owner::Indirect(range) => {
                let by = refs.as_ref().map(|r| r.referencers(range)).unwrap_or_default();
                if by.is_empty() {
                    unreferenced += n as f64;
                }
                for file in &by {
                    *files.entry(*file).or_default() += n as f64 / by.len() as f64;
                }
            }
        }
    }
    Ok((files, unreferenced))
}

/// Samples on @devs by kind, and what the kernel accounts for each: the
/// sectors of its data type, and for unused what's left of the capacity.
fn kind_rows(tally: &Tally, devs: &[usize], usage: &[DevUsage]) -> BTreeMap<Kind, (u64, Option<u64>)> {
    let mut rows: BTreeMap<Kind, (u64, Option<u64>)> = BTreeMap::new();

    for (&(dev, kind), &n) in &tally.kinds {
        if devs.contains(&dev) {
            rows.entry(kind).or_default().0 += n;
        }
    }
    for u in devs.iter().map(|&dev| &usage[dev]) {
        // stripe buckets' data outside the stripe, also counted in stripe
        for (t, d) in u.iter_typed().filter(|(t, d)| d.sectors != 0 && *t != DataType::unstriped) {
            *rows.entry(Kind::data(t)).or_default().1.get_or_insert(0) += d.sectors;
        }
        *rows.entry(Kind::Unused).or_default().1.get_or_insert(0) +=
            u.capacity_sectors().saturating_sub(u.used_sectors());
    }
    rows
}

/// Samples by path, from the filesystem's root.
#[derive(Default)]
struct Tree {
    samples:  f64,
    children: BTreeMap<OsString, Tree>,
}

impl Tree {
    fn add(&mut self, path: &Path, samples: f64) {
        let mut t = self;
        t.samples += samples;
        for name in normal(path) {
            t = t.children.entry(name.to_os_string()).or_default();
            t.samples += samples;
        }
    }

    fn get(&self, path: &Path) -> Option<&Tree> {
        normal(path).try_fold(self, |t, name| t.children.get(name))
    }

    /// The samples below any of @paths, each counted once.
    fn union<'p>(&self, paths: impl Iterator<Item = &'p Path> + Clone) -> f64 {
        paths.clone().enumerate()
            .filter(|&(i, p)| !paths.clone().enumerate()
                .any(|(j, q)| p.starts_with(q) && (p != q || j < i)))
            .filter_map(|(_, p)| self.get(p))
            .map(|t| t.samples)
            .sum()
    }

    /// A line for @path, then for its children down to @depth, largest first.
    fn to_text(&self, out: &mut Printbuf, path: &Path, depth: usize,
               est: &impl Fn(&mut Printbuf, f64)) {
        est(out, self.samples);
        writeln!(out, "{}", path.display());

        if depth > 0 {
            let mut children: Vec<_> = self.children.iter().collect();
            children.sort_by(|a, b| b.1.samples.total_cmp(&a.1.samples));
            for (name, t) in children {
                t.to_text(out, &path.join(name), depth - 1, est);
            }
        }
    }
}

fn normal(path: &Path) -> impl Iterator<Item = &OsStr> {
    path.components().filter_map(|c| match c {
        Component::Normal(name) => Some(name),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space(sizes: &[u64]) -> Space {
        let mut start = 0;
        let devs = sizes.iter().enumerate()
            .map(|(i, &sectors)| {
                let d = DevSpace {
                    idx: i as u32, name: format!("dev{i}"), start, bucket_size: 8,
                    sb: Vec::new(), journal: HashSet::new(),
                };
                start += sectors;
                d
            })
            .collect();
        Space { devs, total: start }
    }

    /// Every sector of the space is one device's, at its own offset.
    #[test]
    fn locate_maps_the_space_onto_devices() {
        let s = space(&[16, 8, 32]);
        assert_eq!(s.locate(0), (0, 0));
        assert_eq!(s.locate(15), (0, 15));
        assert_eq!(s.locate(16), (1, 0));
        assert_eq!(s.locate(23), (1, 7));
        assert_eq!(s.locate(24), (2, 0));
        assert_eq!(s.locate(55), (2, 31));
    }

    /// A path holds what's below it; paths given twice, or one inside
    /// another, count once in the total.
    #[test]
    fn tree_sums_below_and_unions_nested_paths() {
        let mut t = Tree::default();
        t.add(Path::new("/a/b/f"), 3.0);
        t.add(Path::new("/a/g"), 2.0);
        t.add(Path::new("/h"), 1.0);

        assert_eq!(t.samples, 6.0);
        assert_eq!(t.get(Path::new("/a")).map(|t| t.samples), Some(5.0));
        assert_eq!(t.get(Path::new("/a/b")).map(|t| t.samples), Some(3.0));
        assert!(t.get(Path::new("/a/x")).is_none());

        let paths = |p: &'static [&'static str]| p.iter().map(Path::new);
        assert_eq!(t.union(paths(&["/a/b", "/a"])), 5.0);
        assert_eq!(t.union(paths(&["/a", "/a"])), 5.0);
        assert_eq!(t.union(paths(&["/a/b", "/h", "/x"])), 4.0);
        assert_eq!(t.union(paths(&["/", "/a"])), 6.0);
    }

    /// None or all of the samples is certain; the margin is widest at half,
    /// symmetric, and halves with four times the samples.
    #[test]
    fn estimate_scales_a_share_of_samples() {
        let t = |n| Tally { n, ..Default::default() };
        let total = 1 << 20;
        assert_eq!(estimate(&t(1000), total, 0.0), (0, 0));
        assert_eq!(estimate(&t(1000), total, 1000.0), (total, 0));

        let (half, margin) = estimate(&t(1000), total, 500.0);
        assert_eq!(half, total / 2);
        assert_eq!(estimate(&t(1000), total, 250.0).1, estimate(&t(1000), total, 750.0).1);
        assert!(estimate(&t(1000), total, 250.0).1 < margin);
        assert_eq!(estimate(&t(4000), total, 2000.0).1, (margin as f64 / 2.0).round() as u64);
    }
}
