use std::ops::ControlFlow;

use anyhow::Result;
use bch_bindgen::bcachefs;
use bch_bindgen::bkey::BkeySC;
use bch_bindgen::btree::BtreeIter;
use bch_bindgen::btree::BtreeIterFlags;
use bch_bindgen::btree::BtreeNodeIter;
use bch_bindgen::btree::BtreeTrans;
use bch_bindgen::fs::Fs;
use bch_bindgen::opt_set;
use bch_bindgen::c::bch_degraded_actions;
use clap::Parser;
use std::io::{stdout, IsTerminal};

use crate::logging;

fn list_keys(fs: &Fs, opt: &Cli) -> anyhow::Result<()> {
    let trans = BtreeTrans::new(fs);

    let mut flags = BtreeIterFlags::PREFETCH;

    if opt.start.snapshot == 0 {
        flags |= BtreeIterFlags::ALL_SNAPSHOTS;
    }

    let mut iter = BtreeIter::new_level(
        &trans,
        opt.btree,
        opt.start,
        opt.level,
        flags,
    );

    iter.for_each(&trans, |k| {
        if k.k.p > opt.end {
            return ControlFlow::Break(());
        }

        if let Some(ty) = opt.bkey_type {
            if k.k.type_ != ty as u8 {
                return ControlFlow::Continue(());
            }
        }

        println!("{}", k.to_text(fs));
        ControlFlow::Continue(())
    })?;

    Ok(())
}

fn list_btree_formats(fs: &Fs, opt: &Cli) -> anyhow::Result<()> {
    let trans = BtreeTrans::new(fs);
    let mut iter = BtreeNodeIter::new(
        &trans,
        opt.btree,
        opt.start,
        0,
        opt.level,
        BtreeIterFlags::PREFETCH,
    );

    iter.for_each(&trans, |b| {
        if b.key.k.p > opt.end {
            return ControlFlow::Break(());
        }

        println!("{}", b.to_text(fs));
        ControlFlow::Continue(())
    })?;

    Ok(())
}

fn list_btree_nodes(fs: &Fs, opt: &Cli) -> anyhow::Result<()> {
    let trans = BtreeTrans::new(fs);
    let mut iter = BtreeNodeIter::new(
        &trans,
        opt.btree,
        opt.start,
        0,
        opt.level,
        BtreeIterFlags::PREFETCH,
    );

    iter.for_each(&trans, |b| {
        if b.key.k.p > opt.end {
            return ControlFlow::Break(());
        }

        println!("{}", BkeySC::from(&b.key).to_text(fs));
        ControlFlow::Continue(())
    })?;

    Ok(())
}

fn list_nodes_ondisk(fs: &Fs, opt: &Cli) -> anyhow::Result<()> {
    let trans = BtreeTrans::new(fs);
    let mut iter = BtreeNodeIter::new(
        &trans,
        opt.btree,
        opt.start,
        0,
        opt.level,
        BtreeIterFlags::PREFETCH,
    );

    iter.for_each(&trans, |b| {
        if b.key.k.p > opt.end {
            return ControlFlow::Break(());
        }

        println!("{}", b.ondisk_to_text(fs));
        ControlFlow::Continue(())
    })?;

    Ok(())
}

#[derive(Clone, clap::ValueEnum, Debug)]
enum Mode {
    Keys,
    Formats,
    Nodes,
    NodesOndisk,
}

/// List filesystem metadata in textual form
#[derive(Parser, Debug)]
pub struct Cli {
    #[arg(short, long, default_value = "keys")]
    mode: Mode,

    /// Btree to list from
    #[arg(short, long, default_value_t=bcachefs::btree_id::BTREE_ID_extents)]
    btree: bcachefs::btree_id,

    /// Bkey type to list
    #[arg(short = 'k', long)]
    bkey_type: Option<bcachefs::bch_bkey_type>,

    /// Btree depth to descend to (0 == leaves)
    #[arg(short, long, default_value_t = 0)]
    level: u32,

    /// Start position to list from
    #[arg(short, long, default_value = "POS_MIN")]
    start: bcachefs::bpos,

    /// End position
    #[arg(short, long, default_value = "SPOS_MAX")]
    end: bcachefs::bpos,

    /// Check (fsck) the filesystem first
    #[arg(short, long)]
    fsck: bool,

    // FIXME: would be nicer to have `--color[=WHEN]` like diff or ls?
    /// Force color on/off. Default: autodetect tty
    #[arg(short, long, action = clap::ArgAction::Set, default_value_t=stdout().is_terminal())]
    colorize: bool,

    /// Verbose mode
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    #[arg(required(true))]
    devices: Vec<std::path::PathBuf>,
}

fn cmd_list_inner(opt: &Cli) -> anyhow::Result<()> {
    let mut fs_opts = bcachefs::bch_opts::default();

    opt_set!(fs_opts, noexcl, 1);
    opt_set!(fs_opts, nochanges, 1);
    opt_set!(fs_opts, read_only, 1);
    opt_set!(fs_opts, norecovery, 1);
    opt_set!(fs_opts, degraded, bch_degraded_actions::BCH_DEGRADED_very as u8);
    opt_set!(
        fs_opts,
        errors,
        bcachefs::bch_error_actions::BCH_ON_ERROR_continue as u8
    );

    if opt.fsck {
        opt_set!(
            fs_opts,
            fix_errors,
            bcachefs::fsck_err_opts::FSCK_FIX_yes as u8
        );
        opt_set!(fs_opts, norecovery, 0);
    }

    if opt.verbose > 0 {
        opt_set!(fs_opts, verbose, 1);
    }

    // scan for member devices
    let fs = Fs::open(&opt.devices, fs_opts)?;

    match opt.mode {
        Mode::Keys => list_keys(&fs, opt),
        Mode::Formats => list_btree_formats(&fs, opt),
        Mode::Nodes => list_btree_nodes(&fs, opt),
        Mode::NodesOndisk => list_nodes_ondisk(&fs, opt),
    }
}

pub fn list(argv: Vec<String>) -> Result<()> {
    let opt = Cli::parse_from(argv);

    // TODO: centralize this on the top level CLI
    logging::setup(opt.verbose, opt.colorize);

    cmd_list_inner(&opt)
}
