use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use bch_bindgen::c;
use bch_bindgen::c::bch_degraded_actions;
use bch_bindgen::c::bch_persistent_counters::BCH_COUNTER_NR;
use bch_bindgen::fs::Fs;
use bch_bindgen::opt_set;
use bch_bindgen::sb::COUNTERS;
use clap::Parser;

fn match_counter(name: &str) -> Result<usize> {
    COUNTERS.iter().position(|c| c.name == name)
        .ok_or_else(|| anyhow!("invalid counter '{}'", name))
}

#[derive(Parser, Debug)]
#[command(about = "Reset all counters on an unmounted device")]
struct Cli {
    /// Reset specific counters (comma-separated), not all
    #[arg(short, long)]
    counters: Option<String>,

    /// Device path
    device: String,
}

pub fn cmd_reset_counters(argv: Vec<String>) -> Result<()> {
    let cli = Cli::parse_from(argv);

    let to_reset: Vec<usize> = if let Some(ref names) = cli.counters {
        names.split(',')
            .map(|s| match_counter(s.trim()))
            .collect::<Result<Vec<_>>>()?
    } else {
        vec![]
    };

    // scan for member devices
    let scan_opts = c::bch_opts::default();
    let sbs = crate::device_scan::scan_sbs(&cli.device, &scan_opts)?;
    let devs: Vec<PathBuf> = sbs.into_iter().map(|(p, _)| p).collect();

    // open fs in nostart mode
    let mut fs_opts = c::bch_opts::default();
    opt_set!(fs_opts, nostart, 1);
    opt_set!(fs_opts, degraded, bch_degraded_actions::BCH_DEGRADED_very as u8);

    let fs = Fs::open(&devs, fs_opts)
        .context("opening filesystem")?;

    unsafe {
        let now = (*fs.raw).counters.now;

        // zero counters (percpu is just a plain pointer in userspace)
        if to_reset.is_empty() {
            for i in 0..BCH_COUNTER_NR as usize {
                *now.add(i) = 0;
            }
        } else {
            for &i in &to_reset {
                *now.add(i) = 0;
            }
        }

        // persist to superblock
        let _lock = crate::wrappers::sb_lock(fs.raw);
        fs.write_super();
    }

    Ok(())
}
