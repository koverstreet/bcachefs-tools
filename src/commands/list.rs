use std::ops::ControlFlow;

use anyhow::{bail, Result};
use bcachefs_kernel::btree::bkey::{BkeySC, BkeyValSC};
use bcachefs_kernel::btree::iter::BtreeIter;
use bcachefs_kernel::btree::iter::BtreeIterFlags;
use bcachefs_kernel::btree::iter::BtreeNodeIter;
use bcachefs_kernel::btree::iter::BtreeTrans;
use bcachefs_kernel::data::extents;
use bcachefs_kernel::fs::Fs;
use bcachefs_kernel::opt_set;
use bcachefs_kernel::{btree_id, c, pos};
use bch_bindgen::c::bch_degraded_actions;
use clap::Parser;
use serde_json::{json, Value};
use std::io::{stdout, IsTerminal};

use crate::device_scan::OpenedFs;
use crate::logging;
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::online_iter::{OnlineBtreeIter, OnlineIterFlags};

fn bpos_json(p: c::bpos) -> Value {
    let inode = unsafe { core::ptr::addr_of!(p.inode).read_unaligned() };
    let offset = unsafe { core::ptr::addr_of!(p.offset).read_unaligned() };
    let snapshot = unsafe { core::ptr::addr_of!(p.snapshot).read_unaligned() };

    json!({
        "inode": inode,
        "offset": offset,
        "snapshot": snapshot,
    })
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

fn crc32_json(k: &c::bkey, crc: &c::bch_extent_crc32) -> Value {
    json!({
        "compressed_size": crc._compressed_size() + 1,
        "uncompressed_size": crc._uncompressed_size() + 1,
        "live_size": k.size,
        "csum_type": crc.csum_type(),
        "compression_type": crc.compression_type(),
        "offset": crc.offset(),
        "nonce": 0,
        "csum": csum_json(crc.csum as u64, 0),
    })
}

fn crc64_json(k: &c::bkey, crc: &c::bch_extent_crc64) -> Value {
    json!({
        "compressed_size": crc._compressed_size() + 1,
        "uncompressed_size": crc._uncompressed_size() + 1,
        "live_size": k.size,
        "csum_type": crc.csum_type(),
        "compression_type": crc.compression_type(),
        "offset": crc.offset(),
        "nonce": crc.nonce(),
        "csum": csum_json(crc.csum_lo, crc.csum_hi()),
    })
}

fn crc128_json(k: &c::bkey, crc: &c::bch_extent_crc128) -> Value {
    let csum = unsafe { core::ptr::addr_of!(crc.csum).read_unaligned() };
    let lo = u64::from_le(csum.lo);
    let hi = u64::from_le(csum.hi);

    json!({
        "compressed_size": crc._compressed_size() + 1,
        "uncompressed_size": crc._uncompressed_size() + 1,
        "live_size": k.size,
        "csum_type": crc.csum_type(),
        "compression_type": crc.compression_type(),
        "offset": crc.offset(),
        "nonce": crc.nonce(),
        "csum": csum_json(lo, hi),
    })
}

fn extent_entry_type_name(ty: u32) -> &'static str {
    match ty {
        x if x == c::bch_extent_entry_type::BCH_EXTENT_ENTRY_ptr as u32 => "ptr",
        x if x == c::bch_extent_entry_type::BCH_EXTENT_ENTRY_crc32 as u32 => "crc32",
        x if x == c::bch_extent_entry_type::BCH_EXTENT_ENTRY_crc64 as u32 => "crc64",
        x if x == c::bch_extent_entry_type::BCH_EXTENT_ENTRY_crc128 as u32 => "crc128",
        x if x == c::bch_extent_entry_type::BCH_EXTENT_ENTRY_stripe_ptr as u32 => "stripe_ptr",
        x if x == c::bch_extent_entry_type::BCH_EXTENT_ENTRY_rebalance_v1 as u32 => "rebalance_v1",
        x if x == c::bch_extent_entry_type::BCH_EXTENT_ENTRY_flags as u32 => "flags",
        x if x == c::bch_extent_entry_type::BCH_EXTENT_ENTRY_reconcile as u32 => "reconcile",
        x if x == c::bch_extent_entry_type::BCH_EXTENT_ENTRY_reconcile_bp as u32 => "reconcile_bp",
        _ => "unknown",
    }
}

fn bkey_type_name(ty: u8) -> &'static str {
    match ty as u32 {
        x if x == c::bch_bkey_type::KEY_TYPE_deleted.0 => "deleted",
        x if x == c::bch_bkey_type::KEY_TYPE_whiteout.0 => "whiteout",
        x if x == c::bch_bkey_type::KEY_TYPE_error.0 => "error",
        x if x == c::bch_bkey_type::KEY_TYPE_cookie.0 => "cookie",
        x if x == c::bch_bkey_type::KEY_TYPE_hash_whiteout.0 => "hash_whiteout",
        x if x == c::bch_bkey_type::KEY_TYPE_btree_ptr.0 => "btree_ptr",
        x if x == c::bch_bkey_type::KEY_TYPE_extent.0 => "extent",
        x if x == c::bch_bkey_type::KEY_TYPE_reservation.0 => "reservation",
        x if x == c::bch_bkey_type::KEY_TYPE_inode.0 => "inode",
        x if x == c::bch_bkey_type::KEY_TYPE_inode_generation.0 => "inode_generation",
        x if x == c::bch_bkey_type::KEY_TYPE_dirent.0 => "dirent",
        x if x == c::bch_bkey_type::KEY_TYPE_xattr.0 => "xattr",
        x if x == c::bch_bkey_type::KEY_TYPE_alloc.0 => "alloc",
        x if x == c::bch_bkey_type::KEY_TYPE_quota.0 => "quota",
        x if x == c::bch_bkey_type::KEY_TYPE_stripe.0 => "stripe",
        x if x == c::bch_bkey_type::KEY_TYPE_reflink_p.0 => "reflink_p",
        x if x == c::bch_bkey_type::KEY_TYPE_reflink_v.0 => "reflink_v",
        x if x == c::bch_bkey_type::KEY_TYPE_inline_data.0 => "inline_data",
        x if x == c::bch_bkey_type::KEY_TYPE_btree_ptr_v2.0 => "btree_ptr_v2",
        x if x == c::bch_bkey_type::KEY_TYPE_indirect_inline_data.0 => "indirect_inline_data",
        x if x == c::bch_bkey_type::KEY_TYPE_alloc_v2.0 => "alloc_v2",
        x if x == c::bch_bkey_type::KEY_TYPE_subvolume.0 => "subvolume",
        x if x == c::bch_bkey_type::KEY_TYPE_snapshot.0 => "snapshot",
        x if x == c::bch_bkey_type::KEY_TYPE_inode_v2.0 => "inode_v2",
        x if x == c::bch_bkey_type::KEY_TYPE_alloc_v3.0 => "alloc_v3",
        x if x == c::bch_bkey_type::KEY_TYPE_set.0 => "set",
        x if x == c::bch_bkey_type::KEY_TYPE_lru.0 => "lru",
        x if x == c::bch_bkey_type::KEY_TYPE_alloc_v4.0 => "alloc_v4",
        x if x == c::bch_bkey_type::KEY_TYPE_backpointer.0 => "backpointer",
        x if x == c::bch_bkey_type::KEY_TYPE_inode_v3.0 => "inode_v3",
        x if x == c::bch_bkey_type::KEY_TYPE_bucket_gens.0 => "bucket_gens",
        x if x == c::bch_bkey_type::KEY_TYPE_snapshot_tree.0 => "snapshot_tree",
        x if x == c::bch_bkey_type::KEY_TYPE_logged_op_truncate.0 => "logged_op_truncate",
        x if x == c::bch_bkey_type::KEY_TYPE_logged_op_finsert.0 => "logged_op_finsert",
        x if x == c::bch_bkey_type::KEY_TYPE_accounting.0 => "accounting",
        x if x == c::bch_bkey_type::KEY_TYPE_inode_alloc_cursor.0 => "inode_alloc_cursor",
        x if x == c::bch_bkey_type::KEY_TYPE_extent_whiteout.0 => "extent_whiteout",
        x if x == c::bch_bkey_type::KEY_TYPE_logged_op_stripe_update.0 => "logged_op_stripe_update",
        x if x == c::bch_bkey_type::KEY_TYPE_damage.0 => "damage",
        _ => "unknown",
    }
}

fn extent_entries_json(k: BkeySC<'_>) -> Vec<Value> {
    let mut entries = Vec::new();
    let mut current_crc = default_crc_json(k.k.size);

    for entry in extents::bkey_extent_entries_sc(&k.v()) {
        let ty = extents::extent_entry_type(entry);
        let ty_name = extent_entry_type_name(ty);
        let mut out = json!({
            "entry_type": ty,
            "entry_type_name": ty_name,
        });

        match ty {
            x if x == c::bch_extent_entry_type::BCH_EXTENT_ENTRY_ptr as u32 => {
                let ptr = extents::entry_ptr(entry);
                out["ptr"] = json!({
                    "dev": ptr.dev(),
                    "offset": ptr.offset(),
                    "generation": ptr.generation(),
                    "cached": ptr.cached() != 0,
                    "unwritten": ptr.unwritten() != 0,
                    "crc": current_crc.clone(),
                });
            }
            x if x == c::bch_extent_entry_type::BCH_EXTENT_ENTRY_crc32 as u32 => {
                current_crc = crc32_json(k.k, extents::entry_crc32(entry));
                out["crc"] = current_crc.clone();
            }
            x if x == c::bch_extent_entry_type::BCH_EXTENT_ENTRY_crc64 as u32 => {
                current_crc = crc64_json(k.k, extents::entry_crc64(entry));
                out["crc"] = current_crc.clone();
            }
            x if x == c::bch_extent_entry_type::BCH_EXTENT_ENTRY_crc128 as u32 => {
                current_crc = crc128_json(k.k, extents::entry_crc128(entry));
                out["crc"] = current_crc.clone();
            }
            x if x == c::bch_extent_entry_type::BCH_EXTENT_ENTRY_stripe_ptr as u32 => {
                let stripe = extents::entry_stripe_ptr(entry);
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

fn reflink_p_json(v: &c::bch_reflink_p) -> Value {
    let idx_flags = unsafe { core::ptr::addr_of!(v.idx_flags).read_unaligned() };
    let idx_flags = u64::from_le(idx_flags);
    let front_pad = unsafe { core::ptr::addr_of!(v.front_pad).read_unaligned() };
    let back_pad = unsafe { core::ptr::addr_of!(v.back_pad).read_unaligned() };

    json!({
        "idx": idx_flags & ((1u64 << 56) - 1),
        "error": ((idx_flags >> 56) & 1) != 0,
        "may_update_options": ((idx_flags >> 57) & 1) != 0,
        "front_pad": u32::from_le(front_pad),
        "back_pad": u32::from_le(back_pad),
    })
}

fn backpointer_json(v: &c::bch_backpointer) -> Value {
    let flags = unsafe { core::ptr::addr_of!(v.flags).read_unaligned() };
    let bucket_len = unsafe { core::ptr::addr_of!(v.bucket_len).read_unaligned() };
    let pos = unsafe { core::ptr::addr_of!(v.pos).read_unaligned() };

    json!({
        "btree_id": v.btree_id,
        "level": v.level,
        "data_type": v.data_type,
        "bucket_gen": v.bucket_gen,
        "flags": flags,
        "bucket_len": bucket_len,
        "pos": bpos_json(pos),
    })
}

fn key_json(fs: &Fs, btree: c::btree_id, level: u32, k: BkeySC<'_>) -> Value {
    let key_type = k.k.type_;
    let bversion_lo = unsafe { core::ptr::addr_of!(k.k.bversion.lo).read_unaligned() };
    let bversion_hi = unsafe { core::ptr::addr_of!(k.k.bversion.hi).read_unaligned() };

    let mut out = json!({
        "btree": format!("{btree}"),
        "btree_id": btree as u32,
        "level": level,
        "key": {
            "type": key_type,
            "type_name": bkey_type_name(key_type),
            "pos": bpos_json(k.pos()),
            "start_pos": bpos_json(k.start_pos()),
            "size": k.size(),
            "bversion": {
                "lo": bversion_lo,
                "hi": bversion_hi,
            },
            "deleted": k.is_deleted(),
        },
        "text": format!("{}", k.to_text(fs)),
    });

    let entries = extent_entries_json(k);
    if !entries.is_empty() {
        out["extent_entries"] = json!(entries);
    }

    match k.v() {
        BkeyValSC::reflink_p(_, v) => out["reflink_p"] = reflink_p_json(v),
        BkeyValSC::reflink_v(_, v) => {
            let refcount = unsafe { core::ptr::addr_of!(v.refcount).read_unaligned() };
            out["reflink_v"] = json!({ "refcount": u64::from_le(refcount) });
        }
        BkeyValSC::backpointer(_, v) => out["backpointer"] = backpointer_json(v),
        _ => {}
    }

    out
}

fn print_key(fs: &Fs, opt: &Cli, btree: c::btree_id, k: BkeySC<'_>) {
    if opt.json {
        println!("{}", key_json(fs, btree, opt.level, k));
    } else {
        println!("{}", k.to_text(fs));
    }
}

fn list_keys(fs: &Fs, opt: &Cli) -> anyhow::Result<()> {
    let trans = BtreeTrans::new(fs);
    let (btree, start, end) = opt.list_range()?;

    let mut flags = BtreeIterFlags::PREFETCH;

    if start.snapshot == 0 {
        flags |= BtreeIterFlags::ALL_SNAPSHOTS;
    }

    let mut iter = BtreeIter::new_level(&trans, btree, start, opt.level, flags);

    iter.for_each(&trans, |k| {
        if k.k.p > end {
            return ControlFlow::Break(());
        }

        if let Some(ty) = opt.bkey_type {
            if k.k.type_ != ty.0 as u8 {
                return ControlFlow::Continue(());
            }
        }

        print_key(fs, opt, btree, k);
        ControlFlow::Continue(())
    })?;

    Ok(())
}

fn list_btree_formats(fs: &Fs, opt: &Cli) -> anyhow::Result<()> {
    let trans = BtreeTrans::new(fs);
    let (btree, start, end) = opt.list_range()?;

    for level in opt.level..(c::BTREE_MAX_DEPTH as u32) {
        let mut iter = BtreeNodeIter::new(&trans, btree, start, 0, level, BtreeIterFlags::PREFETCH);

        iter.for_each(&trans, |b| {
            if b.key.k.p > end {
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
    let (btree, start, end) = opt.list_range()?;

    for level in opt.level..(c::BTREE_MAX_DEPTH as u32) {
        let mut iter = BtreeNodeIter::new(&trans, btree, start, 0, level, BtreeIterFlags::PREFETCH);

        iter.for_each(&trans, |b| {
            if b.key.k.p > end {
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
    let (btree, start, end) = opt.list_range()?;

    for level in opt.level..(c::BTREE_MAX_DEPTH as u32) {
        let mut iter = BtreeNodeIter::new(&trans, btree, start, 0, level, BtreeIterFlags::PREFETCH);

        iter.for_each(&trans, |b| {
            if b.key.k.p > end {
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
    let (btree, start, end) = opt.list_range()?;
    let mut flags = OnlineIterFlags::default();
    if start.snapshot == 0 {
        flags = flags | OnlineIterFlags::ALL_SNAPSHOTS;
    }

    let mut iter = OnlineBtreeIter::new(handle, btree, opt.level, start, end, flags);

    while let Some(k) = iter
        .next()
        .map_err(|e| anyhow::anyhow!("BCH_IOCTL_QUERY_BTREE_KEYS: {}", e))?
    {
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
        bail!(
            "--fsck requires the filesystem to be unmounted; use 'bcachefs fsck' for online fsck"
        );
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
state, verifying btree contents, and inspecting on-disk layout.\n\n\
Use --inode with the extents btree to inspect the extent keys for one \
file inode, including the pointer/device text printed by the bcachefs \
metadata formatter.")]
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

    /// Limit extents listing to one inode number
    #[arg(long)]
    inode: Option<u64>,

    /// First bpos offset to list with --inode
    #[arg(long, requires = "inode", default_value_t = 0)]
    inode_start_offset: u64,

    /// Last bpos offset to list with --inode
    #[arg(long, requires = "inode", default_value_t = u64::MAX)]
    inode_end_offset: u64,

    /// Emit one JSON object per key
    #[arg(long)]
    json: bool,

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
    fn list_range(&self) -> Result<(c::btree_id, c::bpos, c::bpos)> {
        if let Some(inode) = self.inode {
            if self.inode_start_offset > self.inode_end_offset {
                bail!(
                    "--inode-start-offset ({}) must be <= --inode-end-offset ({})",
                    self.inode_start_offset,
                    self.inode_end_offset
                );
            }

            Ok((
                btree_id::extents,
                pos(inode, self.inode_start_offset),
                pos(inode, self.inode_end_offset),
            ))
        } else {
            Ok((self.btree, self.start, self.end))
        }
    }
}

fn cmd_list_inner(opt: &Cli) -> anyhow::Result<()> {
    let _ = opt.list_range()?;
    if opt.json && !matches!(opt.mode, Mode::Keys) {
        bail!("--json is only supported with keys mode");
    }

    let mut fs_opts = c::bch_opts::default();

    opt_set!(fs_opts, noexcl, 1);
    opt_set!(fs_opts, nochanges, 1);
    opt_set!(fs_opts, read_only, 1);
    opt_set!(fs_opts, norecovery, 1);
    opt_set!(
        fs_opts,
        degraded,
        bch_degraded_actions::BCH_DEGRADED_very as u8
    );
    opt_set!(
        fs_opts,
        errors,
        c::bch_error_actions::BCH_ON_ERROR_continue as u8
    );

    if opt.fsck {
        opt_set!(fs_opts, fix_errors, c::fsck_err_opts::FSCK_FIX_yes as u8);
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

            let devs = handle
                .member_devices()
                .map_err(|e| anyhow::anyhow!("getting member devices from sysfs: {}", e))?;

            opt_set!(fs_opts, nostart, 1);
            let fs = crate::device_scan::open_scan(&devs, fs_opts).map_err(|e| {
                anyhow::anyhow!(
                    "opening {:?} (noexcl/nostart, for formatting keys): {}",
                    devs,
                    e
                )
            })?;

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

#[cfg(test)]
mod tests {
    use super::*;

    fn base_cli() -> Cli {
        Cli {
            mode: Mode::Keys,
            btree: btree_id::extents,
            bkey_type: None,
            level: 0,
            start: pos(0, 0),
            end: pos(u64::MAX, u64::MAX),
            inode: None,
            inode_start_offset: 0,
            inode_end_offset: u64::MAX,
            json: false,
            fsck: false,
            colorize: false,
            verbose: 0,
            devices: Vec::new(),
        }
    }

    #[test]
    fn inode_range_uses_inode_relative_offsets() {
        let mut cli = base_cli();
        cli.inode = Some(123);
        cli.inode_start_offset = 10;
        cli.inode_end_offset = 20;

        let (btree, start, end) = cli.list_range().unwrap();

        assert_eq!(btree, btree_id::extents);
        assert_eq!(start, pos(123, 10));
        assert_eq!(end, pos(123, 20));
    }

    #[test]
    fn inode_range_rejects_reversed_offsets() {
        let mut cli = base_cli();
        cli.inode = Some(123);
        cli.inode_start_offset = 20;
        cli.inode_end_offset = 10;

        assert!(cli.list_range().is_err());
    }

    #[test]
    fn json_flag_parses_for_keys_mode() {
        let cli = Cli::parse_from(["list", "--json", "/dev/null"]);

        assert!(cli.json);
        assert!(matches!(cli.mode, Mode::Keys));
    }

    #[test]
    fn json_rejects_non_key_modes_before_opening_devices() {
        let mut cli = base_cli();
        cli.json = true;
        cli.mode = Mode::Nodes;
        cli.devices.push("/dev/null".into());

        let err = cmd_list_inner(&cli).unwrap_err();

        assert!(err
            .to_string()
            .contains("--json is only supported with keys mode"));
    }
}
