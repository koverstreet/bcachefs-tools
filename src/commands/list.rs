use std::ops::ControlFlow;

use anyhow::{bail, Result};
use bcachefs_kernel::{btree_id, c};
use bcachefs_kernel::btree::bkey::BkeySC;
use bcachefs_kernel::btree::iter::BtreeIter;
use bcachefs_kernel::btree::iter::BtreeIterFlags;
use bcachefs_kernel::btree::iter::BtreeNodeIter;
use bcachefs_kernel::btree::iter::BtreeTrans;
use bcachefs_kernel::fs::Fs;
use bcachefs_kernel::opt_set;
use bch_bindgen::c::bch_degraded_actions;
use clap::Parser;
use std::io::{stdout, IsTerminal};

use crate::logging;
use crate::device_scan::OpenedFs;
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::online_iter::{OnlineBtreeIter, OnlineIterFlags};

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
            if k.k.type_ != ty.0 as u8 {
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
    for level in opt.level..(c::BTREE_MAX_DEPTH as u32) {
        let mut iter = BtreeNodeIter::new(
            &trans,
            opt.btree,
            opt.start,
            0,
            level,
            BtreeIterFlags::PREFETCH,
        );

        iter.for_each(&trans, |b| {
            if b.key.k.p > opt.end {
                return ControlFlow::Break(());
            }

            println!("{}", b.to_text(fs));
            ControlFlow::Continue(())
        })?;
    }

    Ok(())
}

fn list_btree_nodes(fs: &Fs, opt: &Cli) -> anyhow::Result<()> {
    let trans = BtreeTrans::new(fs);
    for level in opt.level..(c::BTREE_MAX_DEPTH as u32) {
        let mut iter = BtreeNodeIter::new(
            &trans,
            opt.btree,
            opt.start,
            0,
            level,
            BtreeIterFlags::PREFETCH,
        );

        iter.for_each(&trans, |b| {
            if b.key.k.p > opt.end {
                return ControlFlow::Break(());
            }

            println!("{}", BkeySC::from(&b.key).to_text(fs));
            ControlFlow::Continue(())
        })?;
    }

    Ok(())
}

fn list_nodes_ondisk(fs: &Fs, opt: &Cli) -> anyhow::Result<()> {
    let trans = BtreeTrans::new(fs);
    for level in opt.level..(c::BTREE_MAX_DEPTH as u32) {
        let mut iter = BtreeNodeIter::new(
            &trans,
            opt.btree,
            opt.start,
            0,
            level,
            BtreeIterFlags::PREFETCH,
        );

        iter.for_each(&trans, |b| {
            if b.key.k.p > opt.end {
                return ControlFlow::Break(());
            }

            println!("{}", b.ondisk_to_text(fs));
            ControlFlow::Continue(())
        })?;
    }

    Ok(())
}

/// List keys from a mounted filesystem: the keys come from the kernel via
/// BCH_IOCTL_QUERY_BTREE_KEYS, and are formatted with a userspace bch_fs
/// opened noexcl|nostart alongside the mount - never started, so the
/// journal is never read; everything key formatting needs (extent entry
/// tables, member names, disk groups) comes from the superblock. Output
/// is identical to the offline path by construction.
fn list_keys_online(handle: &BcachefsHandle, fs: &Fs, opt: &Cli) -> anyhow::Result<()> {
    let mut flags = OnlineIterFlags::default();
    if opt.start.snapshot == 0 {
        flags = flags | OnlineIterFlags::ALL_SNAPSHOTS;
    }

    let mut iter = OnlineBtreeIter::new(handle, opt.btree, opt.level,
					opt.start, opt.end, flags);

    while let Some(k) = iter.next().map_err(|e| anyhow::anyhow!("BCH_IOCTL_QUERY_BTREE_KEYS: {}", e))? {
        if k.k.p > opt.end {
            break;
        }

        if let Some(ty) = opt.bkey_type {
            if k.k.type_ != ty.0 as u8 {
                continue;
            }
        }

        println!("{}", k.to_text(fs));
    }

    Ok(())
}

fn list_online(handle: &BcachefsHandle, fs: &Fs, opt: &Cli) -> anyhow::Result<()> {
    if !matches!(opt.mode, Mode::Keys) {
        bail!("only 'keys' mode is supported on a mounted filesystem");
    }
    if opt.fsck {
        bail!("--fsck requires the filesystem to be unmounted; use 'bcachefs fsck' for online fsck");
    }

    list_keys_online(handle, fs, opt)
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
#[command(long_about = "\
Lists btree contents in human-readable text. Operates on unmounted \
devices in read-only mode; if the filesystem is mounted (device, \
mount point, or UUID), keys are listed via the kernel instead. \
Modes: keys (default) prints key/value pairs, \
formats shows btree node packing format, nodes shows btree node keys, \
nodes-ondisk shows the raw on-disk representation.\n\n\
Use -b to select a btree (default: extents), -s/-e for start/end \
position, -l for btree depth, -k to filter by key type. With -c, \
runs fsck before listing. Output is used for debugging filesystem \
state, verifying btree contents, and inspecting on-disk layout.")]
pub struct Cli {
    #[arg(short, long, default_value = "keys")]
    mode: Mode,

    /// Btree to list from
    #[arg(short, long, default_value_t=btree_id::extents)]
    btree: c::btree_id,

    /// Bkey type to list
    #[arg(short = 'k', long)]
    bkey_type: Option<c::bch_bkey_type>,

    /// Btree depth to descend to (0 == leaves)
    #[arg(short, long, default_value_t = 0)]
    level: u32,

    /// Start position to list from
    #[arg(short, long, default_value = "POS_MIN")]
    start: c::bpos,

    /// End position
    #[arg(short, long, default_value = "SPOS_MAX")]
    end: c::bpos,

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
    let mut fs_opts = c::bch_opts::default();

    opt_set!(fs_opts, noexcl, 1);
    opt_set!(fs_opts, nochanges, 1);
    opt_set!(fs_opts, read_only, 1);
    opt_set!(fs_opts, norecovery, 1);
    opt_set!(fs_opts, degraded, bch_degraded_actions::BCH_DEGRADED_very as u8);
    opt_set!(
        fs_opts,
        errors,
        c::bch_error_actions::BCH_ON_ERROR_continue as u8
    );

    if opt.fsck {
        opt_set!(
            fs_opts,
            fix_errors,
            c::fsck_err_opts::FSCK_FIX_yes as u8
        );
        opt_set!(fs_opts, norecovery, 0);
    }

    if opt.verbose > 0 {
        opt_set!(fs_opts, verbose, 1);
    }

    match crate::device_scan::open_online_or_offline(&opt.devices, fs_opts)? {
        OpenedFs::Online(handle) => {
            // The filesystem is mounted: read keys through the kernel. For
            // formatting them we still want a bch_fs - everything to_text
            // needs is derived from the superblock - so open one
            // noexcl|nostart: no exclusive claim on the mounted devices,
            // never started, journal never read. Opened from the member
            // block devices (from sysfs) - the path we were given may be a
            // mount point or UUID, which aren't openable as devices.
            log::info!("filesystem is mounted, listing via the kernel");

            let devs = handle.member_devices()
                .map_err(|e| anyhow::anyhow!("getting member devices from sysfs: {}", e))?;

            opt_set!(fs_opts, nostart, 1);
            let fs = crate::device_scan::open_scan(&devs, fs_opts)
                .map_err(|e| anyhow::anyhow!(
                    "opening {:?} (noexcl/nostart, for formatting keys): {}", devs, e))?;

            list_online(&handle, &fs, opt)
        }
        OpenedFs::Offline(fs) => match opt.mode {
            Mode::Keys => list_keys(&fs, opt),
            Mode::Formats => list_btree_formats(&fs, opt),
            Mode::Nodes => list_btree_nodes(&fs, opt),
            Mode::NodesOndisk => list_nodes_ondisk(&fs, opt),
        },
    }
}

fn list(opt: Cli) -> Result<()> {

    // TODO: centralize this on the top level CLI
    logging::setup(opt.verbose, opt.colorize);

    cmd_list_inner(&opt)
}

pub const CMD: super::CmdDef = typed_cmd!("list", "List filesystem metadata", Cli, list);
