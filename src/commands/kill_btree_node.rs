// kill_btree_node: Debugging tool for corrupting specific btree nodes.
//
// Walks the btree at a given level and damages the on-disk location of the Nth
// node, simulating media corruption. Used for testing recovery paths — fsck
// should detect and repair the damage.
//
// --error picks what the damage looks like, because recovery takes visibly
// different paths depending on how far a node gets through validation:
//
//   zero  the first block is gone, so nothing parses and there is no evidence
//         the node was ever there beyond the parent's pointer
//   csum  the node header is intact - the parent's btree_ptr_v2 seq matches,
//         min_key and the written count are readable - and one bset fails its
//         checksum. This is the shape field reports arrive in.
//
// Everything on disk after the csum field is covered by the checksum, so any
// byte we change shows up as a checksum error. Injecting a *structural* error
// (bad min_key, wrong seq, bogus u64s) that reaches the validation code as
// itself rather than as a checksum failure means recomputing the checksum
// after the edit - which this doesn't do yet.
//
// Safety: Opens the filesystem read-only (no in-memory modifications), then
// does raw pread()/pwrite() to the block device fd. The O_DIRECT alignment
// constraint comes from the block device being opened with O_DIRECT by the
// kernel code.

use std::ops::ControlFlow;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Result};
use bcachefs_kernel::c;
use bcachefs_kernel::btree::bkey::BkeySC;
use bcachefs_kernel::btree::iter::{BtreeIterFlags, BtreeNodeIter, BtreeTrans};
use bcachefs_kernel::data::extents::bkey_ptrs;
use bcachefs_kernel::opt_set;
use clap::Parser;

struct KillNode {
    btree:  c::btree_id,
    level:  u32,
    idx:    u64,
}

/// What the damage should look like to the read path.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq)]
enum ErrorType {
    /// Zero the first block - nothing parses
    Zero,
    /// Flip a byte of key data - header parses, bset checksum fails
    Csum,
}

/// Make btree nodes unreadable (debugging tool)
#[derive(Parser, Debug)]
#[command(about = "Kill a specific btree node (debugging)")]
pub struct KillBtreeNodeCli {
    /// Node to kill (btree:level:idx)
    #[arg(short, long = "node")]
    nodes: Vec<String>,

    /// Kind of damage to inject
    #[arg(short, long, value_enum, default_value_t = ErrorType::Zero)]
    error: ErrorType,

    /// Device index (default: kill all replicas)
    #[arg(short, long)]
    dev: Option<i32>,

    /// Device(s)
    #[arg(required = true)]
    devices: Vec<PathBuf>,
}

const BTREE_MAX_DEPTH: u32 = 4;

fn parse_kill_node(s: &str) -> Result<KillNode> {
    let parts: Vec<&str> = s.splitn(3, ':').collect();
    if parts.is_empty() {
        bail!("invalid node spec: {}", s);
    }

    let btree: c::btree_id = parts[0].parse()
        .map_err(|_| anyhow!("invalid btree id: {}", parts[0]))?;

    let level = if parts.len() > 1 {
        parts[1].parse::<u32>()
            .map_err(|_| anyhow!("invalid level: {}", parts[1]))?
    } else {
        0
    };

    if level >= BTREE_MAX_DEPTH {
        bail!("invalid level: {} (max {})", level, BTREE_MAX_DEPTH - 1);
    }

    let idx = if parts.len() > 2 {
        parts[2].parse::<u64>()
            .map_err(|_| anyhow!("invalid index: {}", parts[2]))?
    } else {
        0
    };

    Ok(KillNode { btree, level, idx })
}

fn cmd_kill_btree_node(cli: KillBtreeNodeCli) -> Result<()> {

    if cli.nodes.is_empty() {
        bail!("no nodes specified (use -n btree:level:idx)");
    }

    let mut kill_nodes: Vec<KillNode> = cli.nodes.iter()
        .map(|s| parse_kill_node(s))
        .collect::<Result<Vec<_>>>()?;

    let mut fs_opts = c::bch_opts::default();
    opt_set!(fs_opts, read_only, 1);

    let fs = crate::device_scan::open_scan(&cli.devices, fs_opts)?;

    let block_size = fs.opts().block_size as usize;
    let dev_idx = cli.dev.unwrap_or(-1);

    // O_DIRECT requires aligned buffers; bd_fd is opened with O_DIRECT
    let zeroes = crate::util::AlignedBuf::new(block_size);

    // First byte past the btree node header and the first bset's header, i.e.
    // the start of key data: corrupting it fails the bset checksum without
    // also making the header itself unparseable.
    let csum_victim = std::mem::size_of::<c::btree_node>();
    if cli.error == ErrorType::Csum && csum_victim >= block_size {
        bail!("block size {block_size} too small to corrupt key data at offset {csum_victim}");
    }

    let trans = BtreeTrans::new(&fs);

    for kill in &mut kill_nodes {
        let mut found = false;

        let mut iter = BtreeNodeIter::new(
            &trans,
            kill.btree,
            c::bpos::default(),
            0,
            kill.level,
            BtreeIterFlags::empty(),
        );

        iter.for_each(&trans, |b| {
            if b.c.level != kill.level as u8 {
                return ControlFlow::Continue(());
            }

            if kill.idx > 0 {
                kill.idx -= 1;
                return ControlFlow::Continue(());
            }

            found = true;
            let k = BkeySC::from(&b.key);

            for ptr in bkey_ptrs(&b.key) {
                let dev = ptr.dev() as u32;
                if dev_idx >= 0 && dev as i32 != dev_idx {
                    continue;
                }

                let Some(ca) = fs.dev_get(dev) else {
                    continue;
                };

                eprintln!("damaging btree node ({:?}) on dev {} {} l={}\n  {}",
                    cli.error, dev, kill.btree, kill.level, k.to_text(&fs));

                let fd = unsafe { (*ca.disk_sb.bdev).bd_fd };
                let file = crate::wrappers::super_io::borrowed_file(fd);
                let offset = (ptr.offset() as u64) << 9;

                let res = match cli.error {
                    ErrorType::Zero =>
                        std::os::unix::fs::FileExt::write_all_at(&*file, &zeroes, offset),
                    ErrorType::Csum => {
                        let mut buf = crate::util::AlignedBuf::new(block_size);
                        std::os::unix::fs::FileExt::read_exact_at(&*file, &mut buf, offset)
                            .and_then(|()| {
                                buf[csum_victim] ^= 0xff;
                                std::os::unix::fs::FileExt::write_all_at(&*file, &buf, offset)
                            })
                    }
                };
                if let Err(e) = res {
                    eprintln!("error damaging node: {e}");
                }
            }

            ControlFlow::Break(())
        }).map_err(|e| anyhow!("error walking btree nodes: {}", e))?;

        if !found {
            bail!("node at specified index not found");
        }
    }

    Ok(())
}

pub const CMD: super::CmdDef = typed_cmd!("kill_btree_node", "Remove a btree node", KillBtreeNodeCli, cmd_kill_btree_node);
