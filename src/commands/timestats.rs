use std::collections::BTreeMap;
use std::fs;
use std::io::{self, IsTerminal, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, ValueEnum};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{self, ClearType},
};
use owo_colors::OwoColorize;
use serde::{Deserialize, Serialize};

use crate::util::run_tui;
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::sysfs::{dev_name_from_sysfs, sysfs_path_from_fd};

const SYSFS_BASE: &str = "/sys/fs/bcachefs";

// JSON structs matching kernel output from bch2_time_stats_to_json()

#[derive(Deserialize, Serialize, Debug, Clone)]
struct DurationStats {
    min:    u64,
    max:    u64,
    #[serde(default)]
    total:  u64,
    mean:   u64,
    stddev: u64,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
struct EwmaStats {
    mean:   u64,
    stddev: u64,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[allow(dead_code)]
struct TimeStats {
    count:              u64,
    duration_ns:        DurationStats,
    duration_ewma_ns:   EwmaStats,
    between_ns:         DurationStats,
    between_ewma_ns:    EwmaStats,
}

struct StatEntry {
    name:   String,
    stats:  TimeStats,
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
struct BtreeTransFnStats {
    max_mem:            u64,
    duration:           TimeStats,
    #[serde(default)]
    lock_hold_times:    Option<TimeStats>,
}

const NAME_WIDTH: usize = 40;
const COL_WIDTH: usize = 13;

const TIME_UNITS: &[(&str, u64)] = &[
    ("ns", 1), ("us", 1_000), ("ms", 1_000_000), ("s", 1_000_000_000),
];

fn fmt_duration(ns: u64) -> String {
    if ns == 0 { return "0".to_string() }
    let (name, scale) = TIME_UNITS.iter()
        .rev()
        .find(|(_, s)| ns >= s * 10)
        .unwrap_or(&TIME_UNITS[0]);
    format!("{}{}", ns / scale, name)
}

const NUM_COLS: usize = 9;

const COLUMNS: &[&str; NUM_COLS] = &[
    "NAME", "COUNT",
    "DUR_MIN", "DUR_MAX", "DUR_TOTAL",
    "MEAN", "MEAN_RECENT",
    "STDDEV", "STDDEV_RECENT",
];

fn sort_val(e: &StatEntry, col: usize) -> u64 {
    let s = &e.stats;
    match col {
        1 => s.count,
        2 => s.duration_ns.min,
        3 => s.duration_ns.max,
        4 => s.duration_ns.total,
        5 => s.duration_ns.mean,
        6 => s.duration_ewma_ns.mean,
        7 => s.duration_ns.stddev,
        8 => s.duration_ewma_ns.stddev,
        _ => 0,
    }
}

fn sort_entries(entries: &mut [StatEntry], sort_col: usize, reverse: bool) {
    entries.sort_by(|a, b| {
        let ord = if sort_col == 0 {
            a.name.cmp(&b.name)
        } else {
            sort_val(b, sort_col).cmp(&sort_val(a, sort_col))
        };
        if reverse { ord.reverse() } else { ord }
    });
}

fn format_header() -> String {
    COLUMNS.iter().enumerate()
        .map(|(i, &name)| if i == 0 { format!("{:<NAME_WIDTH$}", name) } else { format!("{:>COL_WIDTH$}", name) })
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_row(e: &StatEntry) -> String {
    let s = &e.stats;
    format!("{:<NAME_WIDTH$} {:>COL_WIDTH$} {:>COL_WIDTH$} {:>COL_WIDTH$} {:>COL_WIDTH$} {:>COL_WIDTH$} {:>COL_WIDTH$} {:>COL_WIDTH$} {:>COL_WIDTH$}",
        e.name, s.count,
        fmt_duration(s.duration_ns.min), fmt_duration(s.duration_ns.max), fmt_duration(s.duration_ns.total),
        fmt_duration(s.duration_ns.mean), fmt_duration(s.duration_ewma_ns.mean),
        fmt_duration(s.duration_ns.stddev), fmt_duration(s.duration_ewma_ns.stddev))
}

// Structured data: sections within a filesystem snapshot

struct Section {
    label:   &'static str,
    entries: Vec<StatEntry>,
}

struct FsSnapshot {
    label:    String,
    sections: Vec<Section>,
}

// Sysfs reading

fn find_all_sysfs_dirs() -> Result<Vec<PathBuf>> {
    let base = Path::new(SYSFS_BASE);
    if !base.exists() {
        return Err(anyhow!("No bcachefs filesystems found ({}/ does not exist)", SYSFS_BASE));
    }

    let mut results: Vec<PathBuf> = fs::read_dir(base)
        .context("reading sysfs bcachefs directory")?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();

    if results.is_empty() {
        return Err(anyhow!("No mounted bcachefs filesystems found"));
    }
    results.sort();
    Ok(results)
}

fn read_time_stats(sysfs_path: &Path) -> Result<Vec<StatEntry>> {
    let json_dir = sysfs_path.join("time_stats_json");
    if !json_dir.exists() {
        return Err(anyhow!("time_stats_json not found in {} - kernel may need to be updated",
                           sysfs_path.display()));
    }

    let mut entries = Vec::new();
    for file in fs::read_dir(&json_dir).context("reading time_stats_json directory")? {
        let file = file?;
        let name = file.file_name().to_string_lossy().into_owned();
        let content = fs::read_to_string(file.path())
            .with_context(|| format!("reading {}", file.path().display()))?;
        match serde_json::from_str::<TimeStats>(&content) {
            Ok(stats) => entries.push(StatEntry { name, stats }),
            Err(e) => eprintln!("warning: failed to parse {}: {}", file.path().display(), e),
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

fn read_device_latency_stats(sysfs_path: &Path) -> Result<Vec<StatEntry>> {
    let mut entries = Vec::new();
    let Ok(dir) = fs::read_dir(sysfs_path) else { return Ok(entries) };

    for entry in dir {
        let entry = entry?;
        let dirname = entry.file_name().to_string_lossy().into_owned();
        if !dirname.starts_with("dev-") { continue }

        let dev_path = entry.path();
        let dev_name = dev_name_from_sysfs(&dev_path);

        for (suffix, label) in [("io_latency_stats_read_json", "read"),
                                 ("io_latency_stats_write_json", "write")] {
            let stat_path = dev_path.join(suffix);
            if !stat_path.exists() { continue }
            let content = fs::read_to_string(&stat_path)
                .with_context(|| format!("reading {}", stat_path.display()))?;
            match serde_json::from_str::<TimeStats>(&content) {
                Ok(stats) => entries.push(StatEntry {
                    name: format!("{}/{}", dev_name, label),
                    stats,
                }),
                Err(e) => eprintln!("warning: failed to parse {}: {}", stat_path.display(), e),
            }
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

fn read_btree_trans_stats(sysfs_path: &Path) -> Result<Vec<StatEntry>> {
    let path = sysfs_path.join("internal/btree_trans_stats_json");
    let content = fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let map: BTreeMap<String, BtreeTransFnStats> = serde_json::from_str(&content)
        .with_context(|| format!("parsing {}", path.display()))?;

    let mut entries: Vec<StatEntry> = map.into_iter()
        .map(|(name, s)| StatEntry { name, stats: s.duration })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

// Data collection

fn collect_stats(sysfs_paths: &[PathBuf], show_devices: bool) -> Result<Vec<FsSnapshot>> {
    let mut snaps = Vec::new();
    for path in sysfs_paths {
        let stats = read_time_stats(path)?;
        let (ops, blocked) = stats.into_iter()
            .partition(|e: &StatEntry| !e.name.starts_with("blocked_"));

        let mut sections = vec![
            Section { label: "Operations",  entries: ops },
            Section { label: "Slowpath",    entries: blocked },
        ];
        if show_devices {
            sections.push(Section {
                label:   "Per-device IO latency",
                entries: read_device_latency_stats(path)?,
            });
        }
        if let Ok(entries) = read_btree_trans_stats(path) {
            sections.push(Section {
                label:   "Btree transactions",
                entries,
            });
        }

        snaps.push(FsSnapshot {
            label: path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            sections,
        });
    }
    Ok(snaps)
}

// JSON output

fn print_json(snaps: &[FsSnapshot]) -> Result<()> {
    let mut out: BTreeMap<&str, BTreeMap<&str, &TimeStats>> = BTreeMap::new();
    for snap in snaps {
        let mut map = BTreeMap::new();
        for sec in &snap.sections {
            for e in &sec.entries { map.insert(e.name.as_str(), &e.stats); }
        }
        out.insert(&snap.label, map);
    }
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

// CLI

#[derive(ValueEnum, Clone, Copy, Debug, Default)]
enum SortBy {
    #[default]
    Name,
    Count,
    DurMin,
    DurMax,
    #[value(alias = "total")]
    DurTotal,
    #[value(alias = "mean")]
    MeanSince,
    MeanRecent,
    StddevSince,
    StddevRecent,
}

impl SortBy {
    fn col_index(self) -> usize {
        match self {
            SortBy::Name => 0, SortBy::Count => 1,
            SortBy::DurMin => 2, SortBy::DurMax => 3, SortBy::DurTotal => 4,
            SortBy::MeanSince => 5, SortBy::MeanRecent => 6,
            SortBy::StddevSince => 7, SortBy::StddevRecent => 8,
        }
    }
}

#[derive(Parser, Debug)]
#[command(about = "Display bcachefs time statistics")]
pub struct Cli {
    /// Show stats with zero count
    #[arg(short, long)]
    all: bool,

    /// Sort by column
    #[arg(short, long, default_value = "name")]
    sort: SortBy,

    /// Output raw JSON
    #[arg(long)]
    json: bool,

    /// Skip per-device IO latency stats
    #[arg(long)]
    no_device_stats: bool,

    /// One-shot output (no interactive TUI)
    #[arg(long)]
    once: bool,

    /// Refresh interval in seconds (interactive mode)
    #[arg(short = 'i', long, default_value = "1")]
    interval: f64,

    /// Filesystem UUID, device, or mount point (default: all)
    filesystem: Option<String>,
}

// One-shot display

fn display_stats(snaps: Vec<FsSnapshot>, cli: &Cli) -> Result<()> {
    let multi = snaps.len() > 1;
    let sort_col = cli.sort.col_index();

    for mut snap in snaps {
        if multi { println!("{}:", snap.label); }

        let mut first = true;
        for section in &mut snap.sections {
            if !cli.all { section.entries.retain(|e| e.stats.count > 0); }
            if section.entries.is_empty() { continue }
            sort_entries(&mut section.entries, sort_col, false);

            if !first { println!(); }
            first = false;
            println!("{}:", section.label);
            println!("  {}", format_header());
            for e in &section.entries {
                println!("  {}", format_row(e));
            }
        }
        println!();
    }
    Ok(())
}

// Interactive TUI

struct TuiState {
    sort_col:       usize,
    reverse:        bool,
    show_all:       bool,
    show_devices:   bool,
    paused:         bool,
    interval:       Duration,
    cursor:         usize,
    scroll_offset:  usize,
}

fn format_tui_header(sort_col: usize, reverse: bool) -> String {
    let arrow = if reverse { "\u{25b2}" } else { "\u{25bc}" };
    let mut out = String::from("  ");
    for (i, &name) in COLUMNS.iter().enumerate() {
        if i > 0 { out.push(' '); }
        let col = if i == 0 { format!("{:<NAME_WIDTH$}", name) }
                  else       { format!("{:>COL_WIDTH$}", name) };
        if i == sort_col {
            out.push_str(&format!("{}{}", col.reversed(), arrow.reversed()));
        } else {
            out.push_str(&col);
        }
    }
    out
}

fn build_frame(snaps: &[FsSnapshot], state: &TuiState, multi: bool) -> (Vec<String>, Option<usize>) {
    let mut lines = Vec::new();
    let mut cursor_line = None;
    let mut row = 0usize;

    let pause = if state.paused { " PAUSED" } else { "" };
    lines.push(format!(
        "bcachefs timestats ({}s{})  q:quit  \u{2190}\u{2192}:sort column  r:reverse  a:show all  d:devices  p:pause  1-9:interval",
        state.interval.as_secs(), pause,
    ));
    lines.push(String::new());

    let header = format_tui_header(state.sort_col, state.reverse);

    for snap in snaps {
        if multi { lines.push(format!("{}:", snap.label)); }

        let mut first = true;
        for section in &snap.sections {
            if section.entries.is_empty() { continue }

            if !first { lines.push(String::new()); }
            first = false;
            lines.push(format!("{}:", section.label));
            lines.push(header.clone());
            for entry in &section.entries {
                let row_str = format_row(entry);
                if row == state.cursor {
                    cursor_line = Some(lines.len());
                    lines.push(format!("{}{}", "\u{25ba} ".bold(), row_str.bold()));
                } else {
                    lines.push(format!("  {}", row_str));
                }
                row += 1;
            }
        }
        lines.push(String::new());
    }

    (lines, cursor_line)
}

fn render_frame(
    stdout: &mut io::Stdout,
    snaps: &[FsSnapshot],
    state: &mut TuiState,
    multi: bool,
) -> io::Result<()> {
    let (_, term_h) = terminal::size().unwrap_or((120, 40));
    let visible = (term_h as usize).saturating_sub(1).max(1);

    let (lines, cursor_line) = build_frame(snaps, state, multi);

    if let Some(cl) = cursor_line {
        if cl < state.scroll_offset {
            state.scroll_offset = cl;
        } else if cl >= state.scroll_offset + visible {
            state.scroll_offset = cl - visible + 1;
        }
    }

    execute!(stdout, cursor::MoveTo(0, 0), terminal::Clear(ClearType::All))?;
    for line in lines.iter().skip(state.scroll_offset).take(visible) {
        write!(stdout, "{}\r\n", line)?;
    }
    stdout.flush()
}

fn handle_key(state: &mut TuiState, key: KeyCode, modifiers: KeyModifiers, total_rows: usize) -> bool {
    match key {
        KeyCode::Char('q') | KeyCode::Esc => return true,
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => return true,
        KeyCode::Left   => state.sort_col = state.sort_col.saturating_sub(1),
        KeyCode::Right  => state.sort_col = (state.sort_col + 1).min(NUM_COLS - 1),
        KeyCode::Up     => state.cursor = state.cursor.saturating_sub(1),
        KeyCode::Down   => if total_rows > 0 { state.cursor = (state.cursor + 1).min(total_rows - 1) },
        KeyCode::Char('r') => state.reverse = !state.reverse,
        KeyCode::Char('a') => state.show_all = !state.show_all,
        KeyCode::Char('d') => state.show_devices = !state.show_devices,
        KeyCode::Char('p') => state.paused = !state.paused,
        KeyCode::Char(c @ '1'..='9') => state.interval = Duration::from_secs((c as u64) - ('0' as u64)),
        _ => {}
    }
    false
}

fn run_interactive(cli: Cli, sysfs_paths: Vec<PathBuf>) -> Result<()> {
    let mut state = TuiState {
        sort_col:      cli.sort.col_index(),
        reverse:       false,
        show_all:      cli.all,
        show_devices:  !cli.no_device_stats,
        paused:        false,
        interval:      Duration::from_secs_f64(cli.interval),
        cursor:        0,
        scroll_offset: 0,
    };

    run_tui(|stdout| loop {
        let mut snaps = collect_stats(&sysfs_paths, state.show_devices)
            .unwrap_or_default();
        for snap in &mut snaps {
            for section in &mut snap.sections {
                if !state.show_all {
                    section.entries.retain(|e| e.stats.count > 0);
                }
                sort_entries(&mut section.entries, state.sort_col, state.reverse);
            }
        }
        let total_rows: usize = snaps.iter()
            .flat_map(|s| &s.sections)
            .map(|sec| sec.entries.len()).sum();
        if total_rows > 0 && state.cursor >= total_rows {
            state.cursor = total_rows - 1;
        }

        render_frame(stdout, &snaps, &mut state, sysfs_paths.len() > 1)?;

        if event::poll(state.interval)? {
            if let Event::Key(key) = event::read()? {
                if handle_key(&mut state, key.code, key.modifiers, total_rows) { return Ok(()) }
            }
            while event::poll(Duration::ZERO)? { let _ = event::read()?; }
        }

        if state.paused {
            if let Event::Key(key) = event::read()? {
                if handle_key(&mut state, key.code, key.modifiers, total_rows) { return Ok(()) }
            }
        }
    })
}

// Entry point

pub fn timestats(argv: Vec<String>) -> Result<()> {
    let cli = Cli::parse_from(argv);

    let sysfs_paths: Vec<PathBuf> = if let Some(ref fs_arg) = cli.filesystem {
        let handle = BcachefsHandle::open(fs_arg)
            .with_context(|| format!("opening filesystem '{}'", fs_arg))?;
        vec![sysfs_path_from_fd(handle.sysfs_fd())?]
    } else {
        find_all_sysfs_dirs()?
    };

    if cli.json {
        print_json(&collect_stats(&sysfs_paths, !cli.no_device_stats)?)
    } else if cli.once || !io::stdout().is_terminal() {
        display_stats(collect_stats(&sysfs_paths, !cli.no_device_stats)?, &cli)
    } else {
        run_interactive(cli, sysfs_paths)
    }
}
