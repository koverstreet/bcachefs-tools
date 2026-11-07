//! bcachefs fs du: the space paths on a mounted filesystem take, read from its
//! btrees through BCH_IOCTL_QUERY_BTREE_KEYS, which is admin only.
//!
//! exact.rs walks the paths and reads every file's extents. What it shares
//! with any other reader of a mounted filesystem's btrees lives here: the
//! mounted filesystem, opened once for every path given, and the subvolume to
//! snapshot lookup every snapshot-filtered query starts from.

mod exact;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use bcachefs_kernel::btree::bkey::pos;
use bcachefs_kernel::btree::bkey::prelude::*;
use bcachefs_kernel::c;
use bcachefs_kernel::format::Btree;
use bcachefs_kernel::fs::OpenFs;
use bch_bindgen::c::DegradedAction;
use clap::Parser;

use crate::device_scan::open_mounted_for_decoding;
use crate::util::statx_subvol;
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::online_iter::{OnlineBtreeIter, OnlineIterFlags};

#[derive(Parser, Debug)]
#[command(name = "du", about = "Show space used by files and directories",
    long_about = "Shows, for each path, the space its files reference and the \
device space it occupies, counting each physical extent once: data shared \
between snapshots, reflinked copies and hardlinks is counted once, within a \
path and across all paths in the total. Every replica counts toward disk; \
cached copies and erasure coding parity don't. Directories are walked across \
subvolumes and snapshots, never into another filesystem. Requires \
CAP_SYS_ADMIN.",
    disable_help_flag = true)]
pub struct Cli {
    /// Print help
    #[arg(long = "help", action = clap::ArgAction::Help)]
    _help: (),

    /// Human-readable units
    #[arg(short = 'h', long = "human-readable")]
    human_readable: bool,

    /// Break the total down by compression type
    #[arg(short = 'c', long)]
    compression: bool,

    /// Files and directories on a mounted bcachefs
    #[arg(default_value = ".")]
    paths: Vec<PathBuf>,
}

/// The filesystem the paths are on: a handle for its ioctls, and a bch_fs
/// opened from its superblock for decoding what they return.
struct Mounted {
    handle: BcachefsHandle,
    fs:     OpenFs,
    /// st_dev of the filesystem
    dev:    u64,
}

impl Mounted {
    /// The filesystem every one of @paths is on.
    fn open(paths: &[PathBuf]) -> Result<Self> {
        let (handle, dev) = open_fs_of(&paths[0])?;
        for path in &paths[1..] {
            if open_fs_of(path)?.0.uuid() != handle.uuid() {
                bail!("{}: on a different filesystem than {}", path.display(), paths[0].display());
            }
        }

        let mut opts = c::bch_opts::default();
        opts.set_degraded(DegradedAction::very);
        opts.set_errors(c::bch_error_actions::BCH_ON_ERROR_continue);
        let fs = open_mounted_for_decoding(&handle, opts)?;

        Ok(Mounted { handle, fs, dev })
    }
}

/// The mounted filesystem @path is on, and its st_dev.
fn open_fs_of(path: &Path) -> Result<(BcachefsHandle, u64)> {
    let stx = statx_subvol(path, 0)
        .with_context(|| format!("{}", path.display()))?
        .ok_or_else(|| anyhow!("{}: not on a bcachefs filesystem", path.display()))?;

    // a handle comes from a directory: a regular file is never resolved
    let dir = if u32::from(stx.stx_mode) & libc::S_IFMT == libc::S_IFDIR {
        path
    } else {
        path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."))
    };
    let handle = BcachefsHandle::open_if_mounted(dir)
        .map_err(|e| anyhow!("{}: {e}", dir.display()))?
        .ok_or_else(|| anyhow!("{}: not on a mounted bcachefs", path.display()))?;

    Ok((handle, libc::makedev(stx.stx_dev_major, stx.stx_dev_minor)))
}

/// Subvolume -> its snapshot, looked up once each.
#[derive(Default)]
struct Subvols(HashMap<u64, Option<u32>>);

impl Subvols {
    /// @subvol's snapshot, or None if there's no such subvolume.
    fn snapshot(&mut self, handle: &BcachefsHandle, subvol: u64) -> Result<Option<u32>> {
        if let Some(&snapshot) = self.0.get(&subvol) {
            return Ok(snapshot);
        }

        let p = pos(0, subvol);
        let mut iter = OnlineBtreeIter::with_buf_size(handle, Btree::subvolumes, 0, p, p,
                                                      OnlineIterFlags::default(), 4096);
        let snapshot = match iter.next()? {
            Some(k) if k.k.p == p => Some(k.arm::<c::Subvolume>()
                .ok_or_else(|| anyhow!("subvolume {subvol}: key at {} is a {:?}", k.k.p, k.k.key_type()))?
                .v().snapshot.get()),
            _ => None,
        };

        self.0.insert(subvol, snapshot);
        Ok(snapshot)
    }
}

fn fs_du(cli: Cli) -> Result<()> {
    let mounted = Mounted::open(&cli.paths)?;
    exact::run(&mounted, &cli)
}

pub const CMD: super::CmdDef = typed_cmd!("du", "Show space used by files and directories", Cli, fs_du);
