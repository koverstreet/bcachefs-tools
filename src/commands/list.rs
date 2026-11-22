use std::ops::ControlFlow;

use anyhow::{bail, Result};
use bcachefs_kernel::c;
use bcachefs_kernel::btree::bkey::prelude::*;
use bcachefs_kernel::btree::iter::BtreeIter;
use bcachefs_kernel::btree::iter::BtreeIterFlags;
use bcachefs_kernel::btree::iter::BtreeNodeIter;
use bcachefs_kernel::btree::bkey::{AnyKeySC, BkeySC, KeyState};
use bcachefs_kernel::util::vstructs::Unvalidated;
use bcachefs_kernel::data::ptrs::{extent_entry_type_name, BkeyVPtrs, ExtentEntry, Ptrs};
use bcachefs_kernel::fs::Fs;
use bcachefs_kernel::c::DegradedAction;
use clap::Parser;
use serde_json::{json, Value};
use std::io::{stdout, IsTerminal};

use crate::logging;
use crate::device_scan::OpenedFs;
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::online_iter::{OnlineBtreeIter, OnlineIterFlags};
use bcachefs_kernel::format::{Bpos, Btree, BkeyType};

fn bpos_json(p: Bpos) -> Value {
    let (inode, offset, snapshot) = (p.inode, p.offset, p.snapshot);

    json!({"inode": inode, "offset": offset, "snapshot": snapshot})
}

fn csum_json(lo: u64, hi: u64) -> Value {
    json!({
        "lo": lo,
        "hi": hi,
        "lo_hex": format!("{lo:016x}"),
        "hi_hex": format!("{hi:016x}"),
    })
}

fn default_crc_json(size: u32) -> Value {
    json!({
        "compressed_size": size,
        "uncompressed_size": size,
        "live_size": size,
        "csum_type": 0,
        "compression_type": 0,
        "offset": 0,
        "nonce": 0,
        "csum": csum_json(0, 0),
    })
}

fn crc_json(crc: &c::bch_extent_crc_unpacked) -> Value {
    json!({
        "compressed_size": crc.compressed_size,
        "uncompressed_size": crc.uncompressed_size,
        "live_size": crc.live_size,
        "csum_type": crc.csum_type.get().0,
        "compression_type": crc.compression_type.get().0,
        "offset": crc.offset,
        "nonce": crc.nonce,
        "csum": csum_json(crc.csum.lo.get(), crc.csum.hi.get()),
    })
}

fn extent_entries_json(k: BkeySC<'_, <Unvalidated as KeyState>::Of<Ptrs>>, fs: &Fs) -> Vec<Value> {
    let mut entries = Vec::new();
    let mut current_crc = default_crc_json(k.k.size);
    let mut crcs = k.crcs(fs);

    for entry in k.extent_entries(fs) {
        let ty = entry.ty();
        let mut out = json!({
            "entry_type": ty,
            "entry_type_name": extent_entry_type_name(ty).unwrap_or("unknown"),
        });

        match entry {
            ExtentEntry::ptr(ptr) => {
                out["ptr"] = json!({
                    "dev": ptr.dev(),
                    "offset": ptr.offset(),
                    "generation": ptr.generation(),
                    "cached": ptr.cached() != 0,
                    "unwritten": ptr.unwritten() != 0,
                    "crc": current_crc.clone(),
                });
            }
            ExtentEntry::crc32(_) | ExtentEntry::crc64(_) | ExtentEntry::crc128(_) => {
                current_crc = crc_json(&crcs.next().expect("CRC entry has unpacked value"));
                out["crc"] = current_crc.clone();
            }
            ExtentEntry::stripe_ptr(stripe) => {
                out["stripe_ptr"] = json!({
                    "idx": stripe.idx(),
                    "block": stripe.block(),
                    "redundancy": stripe.redundancy(),
                    "crc": current_crc.clone(),
                });
            }
            _ => {}
        }

        entries.push(out);
    }

    entries
}

fn reflink_p_json(v: &c::ReflinkP) -> Value {
    json!({
        "idx": v.idx(),
        "error": v.error(),
        "may_update_options": v.may_update_options(),
        "front_pad": v.front_pad.get(),
        "back_pad": v.back_pad.get(),
    })
}

fn backpointer_json(v: &c::Backpointer) -> Value {
    json!({
        "btree_id": v.btree_id,
        "level": v.level,
        "data_type": v.data_type,
        "bucket_gen": v.bucket_gen,
        "flags": u32::from(v.flags),
        "bucket_len": v.bucket_len,
        "pos": bpos_json(v.pos),
    })
}

fn key_json(fs: &Fs, btree: Btree, level: u32, k: AnyKeySC<'_, Unvalidated>) -> Value {
    let key_type = k.k.type_;
    let bversion = k.k.bversion;
    let (bversion_lo, bversion_hi) = (bversion.lo, bversion.hi);

    let mut out = json!({
        "btree": format!("{btree}"),
        "btree_id": btree.0,
        "level": level,
        "key": {
            "type": key_type,
            "type_name": BkeyType(key_type as u32).name().unwrap_or("unknown"),
            "pos": bpos_json(k.k.p),
            "start_pos": bpos_json(k.k.start_pos()),
            "size": k.k.size,
            "bversion": {"lo": bversion_lo, "hi": bversion_hi},
            "deleted": BkeyType(key_type as u32).is_deleted(),
        },
        "text": format!("{}", k.to_text(fs)),
    });

    if let Some(ptrs) = k.arm::<Ptrs>() {
        let entries = extent_entries_json(ptrs, fs);
        if !entries.is_empty() {
            out["extent_entries"] = json!(entries);
        }
    }

    if let Some(v) = k.arm::<c::ReflinkP>() {
        out["reflink_p"] = reflink_p_json(v.v());
    } else if let Some(v) = k.arm::<c::ReflinkV>() {
        out["reflink_v"] = json!({"refcount": v.v().refcount.get()});
    } else if let Some(v) = k.arm::<c::Backpointer>() {
        out["backpointer"] = backpointer_json(v.v());
    }

    out
}

fn print_key(fs: &Fs, opt: &Cli, btree: Btree, k: AnyKeySC<'_, Unvalidated>) {
    if opt.json {
        println!("{}", key_json(fs, btree, opt.level, k));
    } else {
        println!("{}", k.to_text(fs));
    }
}

fn list_keys(fs: &Fs, opt: &Cli) -> anyhow::Result<()> {
    let trans = bcachefs_kernel::btree_trans!(fs);
    let (btree, start, end) = opt.list_range()?;

    let mut flags = BtreeIterFlags::PREFETCH;

    if start.snapshot == 0 {
        flags |= BtreeIterFlags::ALL_SNAPSHOTS;
    }

    let mut iter = BtreeIter::new_level(
        &trans,
        btree,
        start,
        opt.level,
        flags,
    );

    iter.for_each(&trans, |_, _, k| {
        if k.k.p > end {
            return Ok(ControlFlow::Break(()));
        }

        if let Some(ty) = opt.bkey_type {
            if k.k.type_ != ty.0 as u8 {
                return Ok(ControlFlow::Continue(()));
            }
        }

        print_key(fs, opt, btree, k.as_read());
        Ok(ControlFlow::Continue(()))
    })?;

    Ok(())
}

fn list_btree_formats(fs: &Fs, opt: &Cli) -> anyhow::Result<()> {
    let trans = bcachefs_kernel::btree_trans!(fs);
    let (btree, start, end) = opt.list_range()?;
    for level in opt.level..c::BTREE_MAX_DEPTH {
        let mut iter = BtreeNodeIter::new(
            &trans,
            btree,
            start,
            0,
            level,
            BtreeIterFlags::PREFETCH,
        );

        iter.for_each(&trans, |b| {
            let contents = b.contents();
            if contents.key.k.p > end {
                return Ok(ControlFlow::Break(()));
            }

            println!("{}", b.to_text(fs));
            Ok(ControlFlow::Continue(()))
        })?;
    }

    Ok(())
}

fn list_btree_nodes(fs: &Fs, opt: &Cli) -> anyhow::Result<()> {
    let trans = bcachefs_kernel::btree_trans!(fs);
    let (btree, start, end) = opt.list_range()?;
    for level in opt.level..c::BTREE_MAX_DEPTH {
        let mut iter = BtreeNodeIter::new(
            &trans,
            btree,
            start,
            0,
            level,
            BtreeIterFlags::PREFETCH,
        );

        iter.for_each(&trans, |b| {
            let contents = b.contents();
            if contents.key.k.p > end {
                return Ok(ControlFlow::Break(()));
            }

            println!("{}", contents.key_i().to_text(fs));
            Ok(ControlFlow::Continue(()))
        })?;
    }

    Ok(())
}

fn list_nodes_ondisk(fs: &Fs, opt: &Cli) -> anyhow::Result<()> {
    let trans = bcachefs_kernel::btree_trans!(fs);
    let (btree, start, end) = opt.list_range()?;
    for level in opt.level..c::BTREE_MAX_DEPTH {
        let mut iter = BtreeNodeIter::new(
            &trans,
            btree,
            start,
            0,
            level,
            BtreeIterFlags::PREFETCH,
        );

        iter.for_each(&trans, |b| {
            let contents = b.contents();
            if contents.key.k.p > end {
                return Ok(ControlFlow::Break(()));
            }

            println!("{}", b.ondisk_to_text(fs));
            Ok(ControlFlow::Continue(()))
        })?;
    }

    Ok(())
}

/// List keys from a mounted filesystem: the keys come from the kernel via
/// BCH_IOCTL_QUERY_BTREE_KEYS, and are formatted with a userspace bch_fs
/// opened noexcl|nostart alongside the mount - never started, so the
/// journal is never read. Static formatting state such as member names and
/// disk groups comes from the superblock; annotations needing live allocator
/// state are omitted.
fn list_keys_online(handle: &BcachefsHandle, fs: &Fs, opt: &Cli) -> anyhow::Result<()> {
    let (btree, start, end) = opt.list_range()?;

    let mut flags = OnlineIterFlags::default();
    if start.snapshot == 0 {
        flags = flags | OnlineIterFlags::ALL_SNAPSHOTS;
    }

    let mut iter = OnlineBtreeIter::new(handle, btree, opt.level, start, end, flags);

    while let Some(k) = iter.next().map_err(|e| anyhow::anyhow!("BCH_IOCTL_QUERY_BTREE_KEYS: {}", e))? {
        if k.k.p > end {
            break;
        }

        if let Some(ty) = opt.bkey_type {
            if k.k.type_ != ty.0 as u8 {
                continue;
            }
        }

        print_key(fs, opt, btree, k);
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
    #[arg(short, long, default_value_t=Btree::extents)]
    btree: Btree,

    /// Bkey type to list
    #[arg(short = 'k', long)]
    bkey_type: Option<BkeyType>,

    /// Emit keys as JSON
    #[arg(long)]
    json: bool,

    /// Btree depth to descend to (0 == leaves)
    #[arg(short, long, default_value_t = 0)]
    level: u32,

    /// Start position to list from
    #[arg(short, long, default_value = "POS_MIN")]
    start: Bpos,

    /// End position
    #[arg(short, long, default_value = "SPOS_MAX")]
    end: Bpos,

    /// Limit extents listing to one inode number
    #[arg(long, conflicts_with_all = ["btree", "start", "end"])]
    inode: Option<u64>,

    /// First bpos offset to list with --inode
    #[arg(long, requires = "inode", default_value_t = 0)]
    inode_start_offset: u64,

    /// Last bpos offset to list with --inode
    #[arg(long, requires = "inode", default_value_t = u64::MAX)]
    inode_end_offset: u64,

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

impl Cli {
    fn list_range(&self) -> Result<(Btree, Bpos, Bpos)> {
        if let Some(inode) = self.inode {
            if self.inode_start_offset > self.inode_end_offset {
                bail!(
                    "--inode-start-offset ({}) must be <= --inode-end-offset ({})",
                    self.inode_start_offset,
                    self.inode_end_offset,
                );
            }

            return Ok((
                Btree::extents,
                Bpos { inode, offset: self.inode_start_offset, snapshot: 0 },
                Bpos { inode, offset: self.inode_end_offset, snapshot: u32::MAX },
            ));
        }

        Ok((self.btree, self.start, self.end))
    }
}

fn cmd_list_inner(opt: &Cli) -> anyhow::Result<()> {
    // Validate ranges and output mode before opening filesystem devices.
    opt.list_range()?;
    if opt.json && !matches!(opt.mode, Mode::Keys) {
        bail!("--json is only supported with keys mode");
    }
    let mut fs_opts = c::bch_opts::default();

    fs_opts.set_noexcl(true);
    fs_opts.set_nochanges(true);
    fs_opts.set_read_only(true);
    fs_opts.set_norecovery(true);
    fs_opts.set_degraded(DegradedAction::very);
    fs_opts.set_errors(c::ErrorAction::continue_);

    if opt.fsck {
        fs_opts.set_fix_errors(c::FsckErrOpt::yes as u8);
        fs_opts.set_norecovery(false);
    }

    if opt.verbose > 0 {
        fs_opts.set_verbose(true);
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

            fs_opts.set_nostart(true);
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
