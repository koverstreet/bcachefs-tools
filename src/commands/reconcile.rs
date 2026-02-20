use std::fmt::Write;
use std::io::{IsTerminal, Write as IoWrite};
use std::time::Duration;
use std::thread;

use anyhow::{anyhow, Result};
use bch_bindgen::c;
use clap::Parser;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{self, ClearType},
};

use crate::util::run_tui;
use crate::wrappers::accounting::{self, DiskAccountingKind};
use crate::wrappers::handle::BcachefsHandle;
use bch_bindgen::printbuf::Printbuf;
use crate::wrappers::sysfs;

use c::bch_reconcile_accounting_type::*;
use c::disk_accounting_type::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "snake_case")]
enum ReconcileType {
    Replicas,
    Checksum,
    ErasureCode,
    Compression,
    Target,
    HighPriority,
    Pending,
    Stripes,
}

impl ReconcileType {
    fn as_c(self) -> c::bch_reconcile_accounting_type {
        match self {
            Self::Replicas     => BCH_RECONCILE_ACCOUNTING_replicas,
            Self::Checksum     => BCH_RECONCILE_ACCOUNTING_checksum,
            Self::ErasureCode  => BCH_RECONCILE_ACCOUNTING_erasure_code,
            Self::Compression  => BCH_RECONCILE_ACCOUNTING_compression,
            Self::Target       => BCH_RECONCILE_ACCOUNTING_target,
            Self::HighPriority => BCH_RECONCILE_ACCOUNTING_high_priority,
            Self::Pending      => BCH_RECONCILE_ACCOUNTING_pending,
            Self::Stripes      => BCH_RECONCILE_ACCOUNTING_stripes,
        }
    }

    fn all() -> Vec<Self> {
        vec![Self::Replicas, Self::Checksum, Self::ErasureCode,
             Self::Compression, Self::Target, Self::HighPriority,
             Self::Pending, Self::Stripes]
    }

    fn all_except_pending() -> Vec<Self> {
        Self::all().into_iter().filter(|t| *t != Self::Pending).collect()
    }
}

/// Show status of background reconciliation
#[derive(Parser, Debug)]
#[command(about = "Show reconcile status", disable_help_flag = true)]
pub struct StatusCli {
    /// Print help
    #[arg(long = "help", action = clap::ArgAction::Help)]
    _help: (),

    /// Reconcile types to display (comma-separated)
    #[arg(short = 't', long = "types", value_delimiter = ',', value_enum)]
    types: Vec<ReconcileType>,

    /// Filesystem mountpoint
    #[arg(default_value = ".")]
    filesystem: String,
}

/// Wait for reconcile to finish background data processing
#[derive(Parser, Debug)]
#[command(about = "Wait for reconcile to finish", disable_help_flag = true)]
pub struct WaitCli {
    /// Print help
    #[arg(long = "help", action = clap::ArgAction::Help)]
    _help: (),

    /// Reconcile types to wait on (comma-separated)
    #[arg(short = 't', long = "types", value_delimiter = ',', value_enum)]
    types: Vec<ReconcileType>,

    /// Filesystem mountpoint
    #[arg(default_value = ".")]
    filesystem: String,
}

/// Query reconcile accounting and format status.
/// Returns true if any work is pending.
fn reconcile_status_to_text(
    out: &mut Printbuf,
    handle: &BcachefsHandle,
    sysfs_path: &std::path::Path,
    types: &[ReconcileType],
) -> Result<bool> {
    let scan_pending = std::fs::read_to_string(sysfs_path.join("reconcile_scan_pending"))
        .map(|s| s.trim().parse::<u64>().unwrap_or(0))
        .unwrap_or(0);

    let result = handle.query_accounting(1 << BCH_DISK_ACCOUNTING_reconcile_work as u32)
        .map_err(|e| anyhow!("query_accounting: {}", e))?;

    // Build array of [data_sectors, metadata_sectors] per reconcile type
    let nr = BCH_RECONCILE_ACCOUNTING_NR as usize;
    let mut v = vec![[0u64; 2]; nr];

    for entry in &result.entries {
        if let DiskAccountingKind::ReconcileWork { work_type } = entry.pos.decode() {
            let idx = work_type as usize;
            if idx < nr {
                v[idx][0] = entry.counter(0);
                v[idx][1] = entry.counter(1);
            }
        }
    }

    out.tabstops(&[32, 12, 12]);
    writeln!(out, "Scan pending:\t{}", scan_pending).unwrap();
    write!(out, "\tdata\rmetadata\r\n").unwrap();

    let mut have_pending = scan_pending != 0;

    for t in types {
        let idx = t.as_c() as usize;
        if idx < nr {
            write!(out, "  ").unwrap();
            accounting::prt_reconcile_type(out, t.as_c());
            write!(out, ":\t").unwrap();
            out.units_sectors(v[idx][0]);
            out.tab_rjust();
            out.units_sectors(v[idx][1]);
            out.tab_rjust();
            out.newline();
            have_pending |= v[idx][0] != 0 || v[idx][1] != 0;
        }
    }

    Ok(have_pending)
}

pub fn cmd_reconcile_status(argv: Vec<String>) -> Result<()> {
    let cli = StatusCli::parse_from(argv);

    let types = if cli.types.is_empty() {
        ReconcileType::all()
    } else {
        cli.types
    };

    let handle = BcachefsHandle::open(&cli.filesystem)
        .map_err(|e| anyhow!("opening filesystem '{}': {}", cli.filesystem, e))?;
    let sysfs_path = sysfs::sysfs_path_from_fd(handle.sysfs_fd())?;

    let mut out = Printbuf::new();
    out.set_human_readable(true);
    reconcile_status_to_text(&mut out, &handle, &sysfs_path, &types)?;

    out.newline();

    // Append kernel reconcile_status from sysfs
    if let Ok(status) = std::fs::read_to_string(sysfs_path.join("reconcile_status")) {
        write!(out, "{}", status).unwrap();
        out.newline();
    }

    print!("{}", out);
    Ok(())
}

pub fn cmd_reconcile_wait(argv: Vec<String>) -> Result<()> {
    let cli = WaitCli::parse_from(argv);

    let types = if cli.types.is_empty() {
        ReconcileType::all_except_pending()
    } else {
        cli.types
    };

    let handle = BcachefsHandle::open(&cli.filesystem)
        .map_err(|e| anyhow!("opening filesystem '{}': {}", cli.filesystem, e))?;
    let sysfs_path = sysfs::sysfs_path_from_fd(handle.sysfs_fd())?;

    // Trigger reconcile wakeup so it starts processing
    let _ = std::fs::write(sysfs_path.join("internal/trigger_reconcile_wakeup"), "1");

    if std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
        reconcile_wait_tui(&handle, &sysfs_path, &types)
    } else {
        reconcile_wait_headless(&handle, &sysfs_path, &types)
    }
}

/// Interactive TUI mode: alternate screen, keyboard input, live updates.
fn reconcile_wait_tui(
    handle: &BcachefsHandle,
    sysfs_path: &std::path::Path,
    types: &[ReconcileType],
) -> Result<()> {
    run_tui(|stdout| loop {
        let mut out = Printbuf::new();
        out.set_human_readable(true);

        let pending = reconcile_status_to_text(&mut out, handle, sysfs_path, types)?;

        execute!(stdout, cursor::MoveTo(0, 0), terminal::Clear(ClearType::All))?;
        write!(stdout, "{}", out)?;
        stdout.flush()?;

        if !pending {
            return Ok(());
        }

        if event::poll(Duration::from_secs(1))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                    _ => {}
                }
            }
            while event::poll(Duration::ZERO)? { let _ = event::read()?; }
        }
    })
}

/// Non-interactive mode: simple polling loop for scripts and CI.
fn reconcile_wait_headless(
    handle: &BcachefsHandle,
    sysfs_path: &std::path::Path,
    types: &[ReconcileType],
) -> Result<()> {
    loop {
        let mut out = Printbuf::new();
        out.set_human_readable(true);

        let pending = reconcile_status_to_text(&mut out, handle, sysfs_path, types)?;

        if !pending {
            return Ok(());
        }

        thread::sleep(Duration::from_secs(1));
    }
}
