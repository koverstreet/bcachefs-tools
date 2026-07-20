//! bcachefs fs failure-domains - what failure domain losses would cost,
//! from the replicas accounting.
//!
//! The replicas accounting gives us, for every distinct replica set in the
//! filesystem, the exact device list and sector count: a complete inventory
//! of what fails together, entirely in userspace.
//!
//! An entry tolerates some number of failed devices among its set:
//!   - replicated data (nr_required <= 1): all but one may fail
//!   - erasure coded stripes (nr_required = nr_data): nr_redundant may fail
//!
//! One table: the failure domains, with what losing each would cost. A
//! nonzero 'lost' cell is a separation violation - data that a single domain
//! failure would take out. Devices with no failure domain set are their own
//! domains, so with none configured this is the per device view.
//!
//! Limits: this is aggregate - it says how much data is exposed, not which
//! extents (that's reconcile's job to find and fix). Extents whose copies
//! are partly in stripes (nr_required = 0 entries) are protected by their
//! stripes and reported via the stripe entries.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as FmtWrite;

use anyhow::{anyhow, Result};
use clap::Parser;

use crate::commands::DeviceNameArgs;
use crate::wrappers::accounting::{AccountingEntry, DiskAccountingKind, data_type, disk_accounting_type};
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::sysfs::{self, DeviceNameMode, DevInfo};
use bcachefs_kernel::util::printbuf::Printbuf;

#[derive(Parser, Debug)]
#[command(name = "failure-domains",
    about = "Show failure domains and what losing each would cost",
    long_about = "Analyzes the replicas accounting to show, for each failure \
domain, the data that would be lost if it failed - too few copies or stripe \
blocks left to reconstruct - and the data that would survive degraded. Lost \
data means failure domain separation is being violated. Devices with no \
failure domain set are their own failure domains.",
    disable_help_flag = true)]
pub struct Cli {
    /// Print help
    #[arg(long = "help", action = clap::ArgAction::Help)]
    _help: (),

    /// Human-readable units
    #[arg(short = 'h', long = "human-readable", default_value = "true")]
    human_readable: bool,

    /// JSON output: sizes in bytes; named domains carry a "devices" count,
    /// single-device rows don't
    #[arg(long = "json")]
    json: bool,

    /// Render a synthetic demo scenario instead of a filesystem:
    /// domains, no-domains, partly-off
    #[arg(long = "demo", hide = true)]
    demo: Option<String>,

    #[command(flatten)]
    device_names: DeviceNameArgs,

    /// Filesystem mountpoint
    #[arg(default_value = ".")]
    mountpoint: String,
}

// ── The exposure engine ──────────────────────────────────────────────

/// One replicas accounting entry, reduced to what failure analysis needs.
struct Entry {
    devs: Vec<u8>,
    /// Failed devices tolerated before data is lost:
    tolerates: u8,
    sectors: u64,
}

struct Exposure {
    entries: Vec<Entry>,
    /// dev idx -> indices into entries, for scoring device sets:
    by_dev: HashMap<u8, Vec<u32>>,
    /// All sectors with durability to lose - replicated + stripes:
    total: u64,
}

fn accounting_replicas_entries(acct: &[&AccountingEntry]) -> Vec<Entry> {
    let mut ret = Vec::new();

    for e in acct {
        let DiskAccountingKind::Replicas { data_type: dt, nr_devs, nr_required, devs } = e.pos.decode()
            else { continue };
        let sectors = e.counter(0);

        // Cached data has no durability to lose; nr_required == 0 extents
        // are backed by stripes, accounted via the stripe entries:
        if sectors == 0 || dt == data_type::cached || nr_required == 0 {
            continue;
        }

        let devs: Vec<u8> = devs[..nr_devs as usize].iter()
            .copied()
            .filter(|&d| d != 255)  /* BCH_SB_MEMBER_INVALID: dead stripe blocks */
            .collect();
        if devs.is_empty() {
            continue;
        }

        // Replicated: survives until every copy is gone. Erasure coded
        // (nr_required = nr_data > 1): survives nr_redundant failures.
        let tolerates = if nr_required > 1 {
            nr_devs - nr_required
        } else {
            devs.len() as u8 - 1
        };

        ret.push(Entry { devs, tolerates, sectors });
    }

    ret
}

/// What failing a device set does, per bucket of sectors:
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Score {
    /// Unreadable - too few copies or stripe blocks survive:
    lost: u64,
    /// Survives, with less redundancy:
    degraded: u64,
}

impl Exposure {
    fn new(acct: &[&AccountingEntry]) -> Exposure {
        Self::from_entries(accounting_replicas_entries(acct))
    }

    fn from_entries(entries: Vec<Entry>) -> Exposure {
        let mut by_dev: HashMap<u8, Vec<u32>> = HashMap::new();
        let mut total = 0;

        for (i, e) in entries.iter().enumerate() {
            total += e.sectors;

            for &d in &e.devs {
                by_dev.entry(d).or_default().push(i as u32);
            }
        }

        Exposure { entries, by_dev, total }
    }

    /// What failing exactly the devices in @f does:
    fn score(&self, f: &[u8]) -> Score {
        let mut seen = HashSet::new();
        let mut s = Score::default();

        for &d in f {
            let Some(list) = self.by_dev.get(&d) else { continue };
            for &i in list {
                if !seen.insert(i) {
                    continue;
                }
                let e = &self.entries[i as usize];
                let nr_failed = e.devs.iter().filter(|d| f.contains(d)).count();
                if nr_failed > e.tolerates as usize {
                    s.lost += e.sectors;
                } else {
                    s.degraded += e.sectors;
                }
            }
        }

        s
    }
}

// ── The failure domains ──────────────────────────────────────────────

/// One row of the report: a named failure domain, or a single device with no
/// failure domain set (which is a domain of its own).
struct Row {
    name: String,
    /// Device count for named domains, None for a lone unlabeled device:
    nr_devs: Option<usize>,
    nr_offline: usize,
    score: Score,
}

/// One row per failure domain: devices sharing a failure_domain string are
/// grouped and scored together; a device with none set is its own single
/// device domain. Named domains first (sorted), then the lone devices.
fn domain_rows(e: &Exposure, devs: &[DevInfo]) -> Vec<Row> {
    let mut domains: BTreeMap<&str, Vec<&DevInfo>> = BTreeMap::new();
    let mut loners: Vec<&DevInfo> = Vec::new();

    for d in devs {
        match d.failure_domain.as_deref() {
            Some(fd) => domains.entry(fd).or_default().push(d),
            None => loners.push(d),
        }
    }

    let mut rows = Vec::new();

    for (name, members) in domains {
        let mut f: Vec<u8> = members.iter().map(|d| d.idx as u8).collect();
        f.sort();
        rows.push(Row {
            name: name.to_string(),
            nr_devs: Some(f.len()),
            nr_offline: members.iter().filter(|d| !d.online).count(),
            score: e.score(&f),
        });
    }

    for d in loners {
        rows.push(Row {
            name: d.dev.clone(),
            nr_devs: None,
            nr_offline: !d.online as usize,
            score: e.score(&[d.idx as u8]),
        });
    }

    rows
}

// ── Output ───────────────────────────────────────────────────────────

fn header_to_text(out: &mut Printbuf, e: &Exposure, devs: &[DevInfo]) {
    let offline = devs.iter().filter(|d| !d.online).count();

    write!(out, "Devices: {}", devs.len()).unwrap();
    if offline != 0 {
        write!(out, " ({} offline)", offline).unwrap();
    }
    writeln!(out).unwrap();

    write!(out, "Data: ").unwrap();
    out.units_sectors(e.total);
    writeln!(out).unwrap();
}

/// The same report as machine-readable JSON: what the tests and scripts
/// consume, so they parse data rather than table formatting. Sizes are in
/// bytes; "devices" is present only on named domain rows.
fn report_to_json(e: &Exposure, devs: &[DevInfo]) -> serde_json::Value {
    let rows = domain_rows(e, devs);

    let domains: Vec<serde_json::Value> = rows.iter().map(|r| {
        let mut o = serde_json::json!({
            "domain":           r.name,
            "offline_devices":  r.nr_offline,
            "lost_bytes":       r.score.lost << 9,
            "degraded_bytes":   r.score.degraded << 9,
        });
        if let Some(n) = r.nr_devs {
            o["devices"] = n.into();
        }
        o
    }).collect();

    serde_json::json!({
        "devices":              devs.len(),
        "offline_devices":      devs.iter().filter(|d| !d.online).count(),
        "data_bytes":           e.total << 9,
        "separation_violated":  rows.iter().any(|r| r.score.lost != 0),
        "domains":              domains,
    })
}

fn report_to_text(out: &mut Printbuf, e: &Exposure, devs: &[DevInfo]) {
    let rows = domain_rows(e, devs);
    let unlabeled = devs.iter().all(|d| d.failure_domain.is_none());

    writeln!(out).unwrap();
    if unlabeled {
        writeln!(out, "No failure domains configured: each device is its own failure domain").unwrap();
    }

    /* The verdict: does any single domain failure lose data? */
    if let Some(worst) = rows.iter().max_by_key(|r| r.score.lost) {
        let what = if unlabeled { "device" } else { "failure domain" };

        if worst.score.lost != 0 {
            write!(out, "Failure domain separation violated: ").unwrap();
            out.units_sectors(worst.score.lost);
            writeln!(out, " lost if {} {} fails", what, worst.name).unwrap();
        } else {
            writeln!(out, "All data survives losing any one {}", what).unwrap();
        }
    }

    writeln!(out, "\nWhat losing each failure domain would cost:").unwrap();
    out.aligned(|sub| {
        writeln!(sub, "domain\tdevices\rlost\rdegraded\r").unwrap();

        for r in rows {
            write!(sub, "{}\t", r.name).unwrap();
            /* online/total: */
            match (r.nr_devs, r.nr_offline) {
                (Some(n), o) => write!(sub, "{}/{}", n - o, n).unwrap(),
                (None, 0)    => (),
                (None, _)    => write!(sub, "0/1").unwrap(),
            }
            write!(sub, "\r").unwrap();
            sub.units_sectors(r.score.lost);
            write!(sub, "\r").unwrap();
            sub.units_sectors(r.score.degraded);
            write!(sub, "\r\n").unwrap();
        }
    });
}

fn fs_failure_domains_to_text(out: &mut Printbuf, cli: &Cli, name_mode: DeviceNameMode) -> Result<()> {
    let path = &cli.mountpoint;

    let handle = BcachefsHandle::open(path)
        .map_err(|e| anyhow!("opening filesystem '{}': {}", path, e))?;
    let sysfs_path = sysfs::sysfs_path_from_fd(handle.sysfs_fd())?;
    let devs = sysfs::fs_get_devices(&sysfs_path, name_mode)?;

    let acct = handle.query_accounting(disk_accounting_type::replicas.bit())
        .map_err(|e| anyhow!("query_accounting ioctl failed (kernel too old?): {}", e))?;
    let acct_refs: Vec<&AccountingEntry> = acct.entries.iter().collect();

    let e = Exposure::new(&acct_refs);
    let uuid = uuid::Uuid::from_bytes(handle.uuid());

    if cli.json {
        let mut j = report_to_json(&e, &devs);
        j["filesystem"] = uuid.hyphenated().to_string().into();
        writeln!(out, "{:#}", j).unwrap();
        return Ok(());
    }

    writeln!(out, "Filesystem: {}", uuid.hyphenated()).unwrap();
    header_to_text(out, &e, &devs);
    report_to_text(out, &e, &devs);
    Ok(())
}

// ── Demo scenarios - synthetic replica sets, no filesystem needed ────

mod demo {
    use super::*;

    /// Deterministic - same output every run:
    struct Rng(u64);

    impl Rng {
        fn next(&mut self, bound: u64) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) % bound
        }
    }

    fn dev(idx: u32, rack: Option<u32>) -> DevInfo {
        DevInfo {
            idx,
            dev: format!("dev{}", idx),
            label: None,
            failure_domain: rack.map(|r| format!("rack{}", r)),
            durability: 1,
            online: true,
        }
    }

    fn entries_from_pairs(pairs: HashMap<[u8; 2], u64>) -> Vec<Entry> {
        pairs.into_iter()
            .map(|(devs, sectors)| Entry {
                devs: devs.to_vec(),
                tolerates: 1,
                sectors,
            })
            .collect()
    }

    /// Six racks of ten, replicas=2 spread correctly: copies always cross
    /// racks. No whole rack failure loses anything.
    fn racks_spread() -> (Vec<Entry>, Vec<DevInfo>) {
        let devs: Vec<DevInfo> = (0..60).map(|i| dev(i, Some(i / 10))).collect();
        let mut rng = Rng(1);
        let mut pairs: HashMap<[u8; 2], u64> = HashMap::new();

        for _ in 0..6000 {
            let a = rng.next(60) as u8;
            let b = loop {
                let b = rng.next(60) as u8;
                if b / 10 != a / 10 {
                    break b;
                }
            };
            let mut key = [a, b];
            key.sort();
            *pairs.entry(key).or_default() += (rng.next(7) + 1) * 2048;
        }

        (entries_from_pairs(pairs), devs)
    }

    /// No failure domains, and the filesystem grew in stages: heavy old
    /// data on the first eight devices, later data spread wider. The old
    /// device pairs carry correlated risk:
    fn grew_in_stages() -> (Vec<Entry>, Vec<DevInfo>) {
        let devs: Vec<DevInfo> = (0..60).map(|i| dev(i, None)).collect();
        let mut rng = Rng(2);
        let mut pairs: HashMap<[u8; 2], u64> = HashMap::new();

        let mut era = |nr_devs: u64, writes: u64, size: u64| {
            for _ in 0..writes {
                let a = rng.next(nr_devs) as u8;
                let b = loop {
                    let b = rng.next(nr_devs) as u8;
                    if b != a {
                        break b;
                    }
                };
                let mut key = [a, b];
                key.sort();
                *pairs.entry(key).or_default() += (rng.next(7) + 1) * size;
            }
        };

        era(8, 3000, 4096);   /* the early days: 8 devices, lots of data */
        era(28, 1500, 2048);  /* first expansion */
        era(60, 800, 2048);   /* current */

        (entries_from_pairs(pairs), devs)
    }

    /// Failure domains, but partly off: a stretch of data was written with
    /// copies landing in one rack (domains misconfigured, a rack's worth of
    /// devices down, data predating the labels...):
    fn partly_off() -> (Vec<Entry>, Vec<DevInfo>) {
        let mut devs: Vec<DevInfo> = (0..60).map(|i| dev(i, Some(i / 10))).collect();

        /* rack5 is down - one of the ways data ends up under-spread: */
        for d in &mut devs[50..60] {
            d.online = false;
        }
        let devs = devs;
        let mut rng = Rng(3);
        let mut pairs: HashMap<[u8; 2], u64> = HashMap::new();

        for i in 0..6000 {
            let a = rng.next(60) as u8;
            let same_rack = i % 8 == 0;
            let b = loop {
                let b = rng.next(60) as u8;
                if b != a && ((b / 10 == a / 10) == same_rack) {
                    break b;
                }
            };
            let mut key = [a, b];
            key.sort();
            *pairs.entry(key).or_default() += (rng.next(7) + 1) * 2048;
        }

        (entries_from_pairs(pairs), devs)
    }

    pub fn scenario(name: &str) -> Option<(Vec<Entry>, Vec<DevInfo>)> {
        match name {
            "domains" => Some(racks_spread()),
            "no-domains" => Some(grew_in_stages()),
            "partly-off" => Some(partly_off()),
            _ => None,
        }
    }
}

fn fs_failure_domains(cli: Cli) -> Result<()> {
    let mut out = Printbuf::new();
    out.set_human_readable(cli.human_readable);
    let name_mode = cli.device_names.name_mode();

    if let Some(name) = &cli.demo {
        let (entries, devs) = demo::scenario(name)
            .ok_or_else(|| anyhow!("unknown scenario '{}' (have: domains, no-domains, partly-off)", name))?;

        let e = Exposure::from_entries(entries);
        if cli.json {
            writeln!(out, "{:#}", report_to_json(&e, &devs)).unwrap();
        } else {
            writeln!(out, "Demo scenario: {}", name).unwrap();
            header_to_text(&mut out, &e, &devs);
            report_to_text(&mut out, &e, &devs);
        }
    } else {
        fs_failure_domains_to_text(&mut out, &cli, name_mode)?;
    }
    print!("{}", out);
    Ok(())
}

pub const CMD: super::CmdDef = typed_cmd!("failure-domains",
    "Show failure domains and what losing each would cost",
    Cli, fs_failure_domains);

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(devs: &[u8], tolerates: u8, sectors: u64) -> Entry {
        Entry { devs: devs.to_vec(), tolerates, sectors }
    }

    /// Two racks of two devices ({0,2} and {1,3}), replicas=2 spread
    /// correctly: every entry crosses racks.
    fn cross_rack() -> Exposure {
        Exposure::from_entries(vec![
            entry(&[0, 1], 1, 100),
            entry(&[0, 3], 1, 100),
            entry(&[1, 2], 1, 100),
            entry(&[2, 3], 1, 100),
        ])
    }

    fn rack_devs() -> Vec<DevInfo> {
        (0..4).map(|i| DevInfo {
            idx: i,
            dev: format!("dev{}", i),
            label: None,
            failure_domain: Some(format!("rack{}", i % 2)),
            durability: 1,
            online: true,
        }).collect()
    }

    #[test]
    fn spread_replicas() {
        let e = cross_rack();

        /* failing a whole rack: everything survives, degraded: */
        assert_eq!(e.score(&[0, 2]),
                   Score { lost: 0, degraded: 400 });

        /* a cross rack pair kills its entry, degrades the overlapping
         * entries, never touches the disjoint one: */
        assert_eq!(e.score(&[0, 1]),
                   Score { lost: 100, degraded: 200 });
    }

    #[test]
    fn domains() {
        let rows = domain_rows(&cross_rack(), &rack_devs());

        /* two racks, nothing lost if either fails whole: */
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "rack0");
        assert_eq!(rows[0].nr_devs, Some(2));
        assert_eq!(rows[0].score, Score { lost: 0, degraded: 400 });
        assert_eq!(rows[1].name, "rack1");
        assert_eq!(rows[1].score.lost, 0);
    }

    #[test]
    fn violation_shows_as_lost() {
        /* both copies in rack 0 (devices 0 and 2): */
        let e = Exposure::from_entries(vec![
            entry(&[0, 2], 1, 100),
            entry(&[0, 1], 1, 100),
        ]);

        let rows = domain_rows(&e, &rack_devs());

        assert_eq!(rows[0].name, "rack0");
        assert_eq!(rows[0].score.lost, 100);
    }

    #[test]
    fn ec_stripe() {
        /* one 4+2 stripe across six devices: */
        let e = Exposure::from_entries(vec![entry(&[0, 1, 2, 3, 4, 5], 2, 600)]);

        assert_eq!(e.score(&[0]), Score { lost: 0, degraded: 600 });
        assert_eq!(e.score(&[0, 1]), Score { lost: 0, degraded: 600 });
        assert_eq!(e.score(&[0, 1, 2]), Score { lost: 600, degraded: 0 });
    }

    #[test]
    fn unlabeled_devices_are_domains() {
        let e = Exposure::from_entries(vec![entry(&[0, 1], 1, 100)]);
        let devs: Vec<DevInfo> = (0..2).map(|i| DevInfo {
            idx: i,
            dev: format!("dev{}", i),
            label: None,
            failure_domain: None,
            durability: 1,
            online: true,
        }).collect();

        let rows = domain_rows(&e, &devs);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "dev0");
        assert_eq!(rows[0].nr_devs, None);
        assert_eq!(rows[0].score, Score { lost: 0, degraded: 100 });
    }
}
