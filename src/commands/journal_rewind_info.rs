use std::fmt::Write;

use anyhow::{anyhow, bail, Result};
use bch_bindgen::c;
use bcachefs_kernel::opt_set;
use chrono::{TimeZone, Utc};
use clap::Parser;

use crate::wrappers::journal::JournalReplays;

// ---- per-jset accessors ----

/// A jset's first datetime entry's seconds-since-epoch, or None if absent.
fn jset_datetime(j: &c::jset) -> Option<u64> {
    j.entries().find_map(|e| match e.typed() {
        Ok(c::JsetEntryRef::datetime(d)) => Some(d.seconds().get()),
        _ => None,
    })
}

/// A jset's first rewind_limit entry's seq, or None if absent.
fn jset_rewind_limit(j: &c::jset) -> Option<u64> {
    j.entries().find_map(|e| match e.typed() {
        Ok(c::JsetEntryRef::rewind_limit(r)) => Some(r.seq().get()),
        _ => None,
    })
}

fn fmt_secs(secs: u64) -> String {
    match Utc.timestamp_opt(secs as i64, 0) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        _ => format!("(invalid: {secs})"),
    }
}

// ---- CLI ----

/// Show journal rewind candidates: range of safe rewind seqs and the
/// flush entries within that range (each is a valid rewind target).
#[derive(Parser, Debug)]
#[command(name = "journal_rewind_info")]
pub struct Cli {
    /// Additional mount options
    #[arg(short = 'o')]
    opts: Vec<String>,

    /// Maximum number of flush candidates to list (0 = unlimited)
    #[arg(short = 'n', long, default_value_t = 0)]
    nr: usize,

    /// Verbose mount output
    #[arg(short = 'v', long)]
    verbose: bool,

    /// Devices
    #[arg(required = true)]
    devices: Vec<String>,
}

fn cmd_journal_rewind_info(cli: Cli) -> Result<()> {
    let mut opts = bcachefs_kernel::opts::parse_mount_opts_vec(&cli.opts, false)
        .map_err(|e| anyhow!("error parsing options: {}", crate::wrappers::bch_err_str(e.raw())))?;
    opt_set!(opts, noexcl, 1);
    opt_set!(opts, nochanges, 1);
    opt_set!(opts, norecovery, 1);
    opt_set!(opts, read_only, 1);
    opt_set!(opts, degraded, c::bch_degraded_actions::BCH_DEGRADED_very.0 as u8);
    opt_set!(opts, errors, c::bch_error_actions::BCH_ON_ERROR_continue.0 as u8);
    opt_set!(opts, fix_errors, c::fsck_err_opts::FSCK_FIX_yes as u8);
    opt_set!(opts, retain_recovery_info, 1);
    opt_set!(opts, read_journal_only, 1);
    opt_set!(opts, read_entire_journal, 1);
    if cli.verbose {
        opt_set!(opts, verbose, 1);
    }

    if cli.devices.is_empty() {
        bail!("Please supply device(s) to open");
    }

    let devs: Vec<std::path::PathBuf> = cli.devices.iter().map(std::path::PathBuf::from).collect();
    let fs = crate::device_scan::open_scan(&devs, opts)
        .map_err(|e| anyhow!("error opening {}: {}", cli.devices[0], e))?;

    let replays = JournalReplays::collect(&fs);
    if replays.is_empty() {
        bail!("no journal entries found");
    }

    // Find the most recent entry (max seq).
    let latest = replays.iter().max_by_key(|r| r.seq()).unwrap();
    let latest_j = latest.jset();
    let latest_seq = latest_j.seq.get();
    let latest_dt = jset_datetime(latest_j);

    // The rewind floor comes from the latest entry's rewind_limit sub-entry.
    // If absent (older fs format / not yet written): fall back to lowest
    // seq present on disk and warn.
    let (floor_seq, fell_back) = match jset_rewind_limit(latest_j) {
        Some(s) => (s, false),
        None => {
            let lo = replays.iter().map(|r| r.seq()).min().unwrap();
            (lo, true)
        }
    };

    let mut out = String::new();

    if fell_back {
        writeln!(out,
            "warning: most recent journal entry has no rewind_limit sub-entry;\n\
             \x20        falling back to lowest seq present on disk."
        ).unwrap();
    }

    writeln!(out, "rewind limit:  seq {}  (oldest safe)", floor_seq).unwrap();
    write!(out,   "newest:        seq {}", latest_seq).unwrap();
    if let Some(s) = latest_dt {
        write!(out, "  {}", fmt_secs(s)).unwrap();
    }
    writeln!(out).unwrap();
    writeln!(out).unwrap();

    // Walk entries in window, filter to flush entries.
    let mut candidates: Vec<(u64, Option<u64>)> = Vec::new();
    for r in replays.iter() {
        let s = r.seq();
        if s < floor_seq || s > latest_seq {
            continue;
        }
        if r.j.no_flush() {
            continue;
        }
        candidates.push((s, jset_datetime(r.jset())));
    }
    candidates.sort_by_key(|(s, _)| *s);

    let total_entries_in_window = replays.iter()
        .map(|r| r.seq())
        .filter(|s| *s >= floor_seq && *s <= latest_seq)
        .count();

    writeln!(out, "rewind candidates (flush entries):").unwrap();
    if candidates.is_empty() {
        writeln!(out, "  (none — window contains no flush entries)").unwrap();
    } else {
        let limit = if cli.nr == 0 { candidates.len() } else { cli.nr.min(candidates.len()) };

        // If truncating, prefer showing the most-recent N; that's what a
        // recovery operator usually wants ("what can I rewind to from
        // close to now?").
        let truncated = candidates.len() > limit;
        let start = candidates.len() - limit;
        if truncated {
            writeln!(out, "  ({} earlier candidates omitted; use -n 0 to see all)",
                     candidates.len() - limit).unwrap();
        }
        for (s, dt) in &candidates[start..] {
            match dt {
                Some(secs) => writeln!(out, "  seq {:<12}  {}", s, fmt_secs(*secs)).unwrap(),
                None       => writeln!(out, "  seq {:<12}  (no datetime)", s).unwrap(),
            }
        }
    }

    writeln!(out).unwrap();
    writeln!(out, "  {} flush candidates across {} entries in window",
             candidates.len(), total_entries_in_window).unwrap();

    print!("{out}");
    Ok(())
}

pub const CMD: super::CmdDef = typed_cmd!(
    "journal_rewind_info",
    "Show how far back the journal can be rewound, and the flush points within that window",
    Cli,
    cmd_journal_rewind_info
);
