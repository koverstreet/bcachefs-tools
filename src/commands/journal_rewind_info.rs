use std::fmt::Write;

use anyhow::{anyhow, bail, Result};
use bch_bindgen::c;
use bch_bindgen::journal::{entry_type, jset_entries, jset_no_flush};
use bch_bindgen::opt_set;
use chrono::{TimeZone, Utc};
use clap::Parser;

// ---- RAII wrapper, same shape as list_journal ----

struct JournalEntries {
    inner: c::rust_journal_entries,
}

impl JournalEntries {
    fn collect(c_fs: *mut c::bch_fs) -> Self {
        Self { inner: unsafe { c::rust_collect_journal_entries(c_fs) } }
    }

    fn as_slice(&self) -> &[*mut c::journal_replay] {
        if self.inner.entries.is_null() || self.inner.nr == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(self.inner.entries, self.inner.nr) }
        }
    }
}

impl Drop for JournalEntries {
    fn drop(&mut self) {
        if !self.inner.entries.is_null() {
            unsafe { libc::free(self.inner.entries as *mut _) }
        }
    }
}

// ---- jset_entry sub-struct field readers ----
//
// struct jset_entry is 8 bytes (u64s, btree_id, level, type, pad[3]); the
// type-specific payload starts at offset 8. We read those payload fields
// raw because bch_bindgen doesn't expose jset_entry_{datetime,rewind_limit}.

fn entry_payload_le64(entry: &c::jset_entry) -> u64 {
    unsafe {
        let p = (entry as *const c::jset_entry as *const u8).add(8) as *const u64;
        u64::from_le(p.read_unaligned())
    }
}

/// Extract the seq from a jset_entry of type rewind_limit (oldest safe rewind).
fn entry_rewind_limit_seq(entry: &c::jset_entry) -> u64 {
    entry_payload_le64(entry)
}

/// Extract seconds-since-epoch from a jset_entry of type datetime.
fn entry_datetime_seconds(entry: &c::jset_entry) -> u64 {
    entry_payload_le64(entry)
}

// ---- per-replay accessors ----

fn jset_seq(p: &c::journal_replay) -> u64 {
    u64::from_le(p.j.seq)
}

/// Scan a jset for its first BCH_JSET_ENTRY_datetime sub-entry and return
/// seconds-since-epoch, or None if absent.
fn jset_datetime(p: &c::journal_replay) -> Option<u64> {
    for e in jset_entries(&p.j) {
        if entry_type(e) == Some(c::bch_jset_entry_type::BCH_JSET_ENTRY_datetime) {
            return Some(entry_datetime_seconds(e));
        }
    }
    None
}

/// Scan a jset for its first BCH_JSET_ENTRY_rewind_limit and return the seq.
fn jset_rewind_limit(p: &c::journal_replay) -> Option<u64> {
    for e in jset_entries(&p.j) {
        if entry_type(e) == Some(c::bch_jset_entry_type::BCH_JSET_ENTRY_rewind_limit) {
            return Some(entry_rewind_limit_seq(e));
        }
    }
    None
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
    let mut opts = bch_bindgen::opts::parse_mount_opts_vec(&cli.opts, false)
        .map_err(|e| anyhow!("error parsing options: {}", crate::wrappers::bch_err_str(e.raw())))?;
    opt_set!(opts, noexcl, 1);
    opt_set!(opts, nochanges, 1);
    opt_set!(opts, norecovery, 1);
    opt_set!(opts, read_only, 1);
    opt_set!(opts, degraded, c::bch_degraded_actions::BCH_DEGRADED_very as u8);
    opt_set!(opts, errors, c::bch_error_actions::BCH_ON_ERROR_continue as u8);
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

    let c_fs = fs.raw;

    let je = JournalEntries::collect(c_fs);
    let entries = je.as_slice();
    if entries.is_empty() {
        bail!("no journal entries found");
    }

    // Find the most recent entry (max seq).
    let latest_p: &c::journal_replay = entries.iter()
        .map(|&ep| unsafe { &*ep })
        .max_by_key(|p| jset_seq(p))
        .unwrap();
    let latest_seq = jset_seq(latest_p);
    let latest_dt = jset_datetime(latest_p);

    // The rewind floor comes from the latest entry's rewind_limit sub-entry.
    // If absent (older fs format / not yet written): fall back to lowest
    // seq present on disk and warn.
    let (floor_seq, fell_back) = match jset_rewind_limit(latest_p) {
        Some(s) => (s, false),
        None => {
            let lo = entries.iter()
                .map(|&ep| unsafe { jset_seq(&*ep) })
                .min()
                .unwrap();
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
    for &ep in entries {
        let p = unsafe { &*ep };
        let s = jset_seq(p);
        if s < floor_seq || s > latest_seq {
            continue;
        }
        if jset_no_flush(&p.j) {
            continue;
        }
        candidates.push((s, jset_datetime(p)));
    }
    candidates.sort_by_key(|(s, _)| *s);

    let total_entries_in_window = entries.iter()
        .map(|&ep| unsafe { jset_seq(&*ep) })
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
