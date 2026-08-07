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
//         checksum, with every key in it still byte-for-byte correct. This is
//         the shape field reports arrive in, and it isolates the read path's
//         accept-or-reject decision from any question of key damage.
//   keys  same, but the flipped byte is in the key data, so the bset fails its
//         checksum *and* holds a corrupt key. This is what exercises per-key
//         validation, which is the only thing between an accepted bad-checksum
//         bset and the btree.
//
// The checksum covers [sizeof(bch_csum), vstruct_end), and vstruct_end is
// sizeof(btree_node) + keys.u64s * 8 - not the whole block. So the byte to flip
// has to be one the bset actually reaches: presplit_shard_boundaries leaves
// plenty of nodes whose first bset is empty, and on those the first key byte is
// already past the end, so flipping it changes nothing and the injection is a
// silent no-op.
//
// Anything that stops the requested damage from landing is a hard error here,
// never a warning: this is a test instrument, and a caller that asked for
// corruption and silently didn't get it goes on to "verify" recovery against an
// intact filesystem and passes. Exit status is the only signal a shell script
// gets, so it has to mean "the node is damaged".
//
// Injecting a *structural* error (bad min_key, wrong seq, bogus u64s) that
// reaches the validation code as itself rather than as a checksum failure means
// recomputing the checksum after the edit - which this doesn't do yet.
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
    /// Flip a bset header byte - checksum fails, every key still intact
    Csum,
    /// Flip a byte inside key data - checksum fails and one key is corrupt
    Keys,
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

/// Pick the byte to flip for --error csum: the journal_seq of the first bset in
/// the node that actually holds keys. Returns (byte offset, the bset's sector
/// offset within the node, its u64s).
///
/// journal_seq because it sits in the bset header, so it's covered by the
/// checksum whatever u64s is, and nothing reads it before the checksum is
/// verified - the node header stays parseable and the parent's btree_ptr_v2 seq
/// still matches.
///
/// Skipping empty bsets matters: the first bset comes back u64s 0 not just on
/// freshly split nodes but on plenty of populated ones, and corrupting an empty
/// bset gives the read path nothing to decide - no key is at risk, so the
/// accept-vs-reject behaviour we're trying to exercise never runs.
///
/// Layout is what bch2_btree_node_read_done() walks: the first bset is the
/// btree_node itself, each later one a btree_node_entry at the running sector
/// offset, each occupying vstruct_sectors i.e. round_up(header + u64s * 8,
/// block_size).
struct BsetLoc {
    /// byte offset of the bset within the node
    at:     usize,
    /// sector offset of its container, for reporting (matches the read path's
    /// "node offset N/written")
    sector: usize,
    u64s:   usize,
}

/// The node's first bset, keys or not.
///
/// --error csum doesn't need a key to be at stake, and a node whose bsets are all
/// empty still has to be corruptible: on a freshly formatted filesystem every
/// btree leaf is one of those, which is exactly what the spin repro formats.
fn first_bset(node: &[u8]) -> BsetLoc {
    let at = size_of::<c::btree_node>() - size_of::<c::bset>();
    let u64s_at = at + std::mem::offset_of!(c::bset, u64s);

    BsetLoc {
        at,
        sector: 0,
        u64s: u16::from_le_bytes([node[u64s_at], node[u64s_at + 1]]) as usize,
    }
}

fn first_bset_with_keys(node: &[u8], written_sectors: usize, block_size: usize)
    -> Option<BsetLoc>
{
    let u64s_in_bset = std::mem::offset_of!(c::bset, u64s);

    let mut off = 0;

    while off < written_sectors * 512 {
        // offset of the bset within its container, and the container's size -
        // which is offsetof(_data), i.e. where this bset's keys begin
        let (bset_at, header) = if off == 0 {
            (size_of::<c::btree_node>() - size_of::<c::bset>(), size_of::<c::btree_node>())
        } else {
            (off + size_of::<c::btree_node_entry>() - size_of::<c::bset>(),
             size_of::<c::btree_node_entry>())
        };

        let u64s_at = bset_at + u64s_in_bset;
        if u64s_at + 2 > node.len() {
            break;
        }

        let u64s = u16::from_le_bytes([node[u64s_at], node[u64s_at + 1]]) as usize;

        if u64s != 0 {
            return Some(BsetLoc { at: bset_at, sector: off / 512, u64s });
        }

        /* empty bset: skip past it and try the next */
        let sectors = (header + u64s * 8).div_ceil(block_size) * block_size / 512;
        if sectors == 0 {
            break;
        }
        off += sectors * 512;
    }

    None
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

    let trans = BtreeTrans::new(&fs);

    for kill in &mut kill_nodes {
        let mut found = false;
        let mut damaged = 0;
        let mut damage_err: Option<anyhow::Error> = None;

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

                let damage = || -> Result<()> {
                    match cli.error {
                        ErrorType::Zero =>
                            std::os::unix::fs::FileExt::write_all_at(&*file, &zeroes, offset)?,
                        ErrorType::Csum | ErrorType::Keys => {
                            /* the whole written extent, so we can walk its bsets */
                            let written = b.written as usize;
                            let mut buf = crate::util::AlignedBuf::new((written * 512).max(block_size));

                            std::os::unix::fs::FileExt::read_exact_at(&*file, &mut buf, offset)?;

                            let bset = first_bset_with_keys(&buf, written, block_size)
                                .or_else(|| (cli.error == ErrorType::Csum)
                                            .then(|| first_bset(&buf)))
                                .ok_or_else(|| anyhow!(
                                    "no bset with keys in this {written} sector node, so \
                                     --error keys has nothing to corrupt"))?;

                            let (victim, what) = if cli.error == ErrorType::Csum {
                                (bset.at + std::mem::offset_of!(c::bset, journal_seq),
                                 "bset header")
                            } else {
                                /* midway into the keys: inside the checksummed
                                 * region, and corrupts a key rather than a
                                 * header field the keys don't depend on */
                                (bset.at + size_of::<c::bset>() + bset.u64s * 8 / 2,
                                 "key data")
                            };

                            if victim >= buf.len() {
                                bail!("bset at node offset {} claims u64s {}, putting the byte \
                                       to corrupt ({victim}) past the end of the {} byte node",
                                      bset.sector, bset.u64s, buf.len());
                            }

                            eprintln!("  corrupting {what} of bset at node offset {}/{written}, u64s {}",
                                      bset.sector, bset.u64s);

                            buf[victim] ^= 0xff;
                            std::os::unix::fs::FileExt::write_all_at(&*file, &buf, offset)?;
                        }
                    }
                    Ok(())
                };

                /*
                 * Stop on the first failure rather than carrying on: a caller
                 * that asked for damage and didn't get it will otherwise go on
                 * to "verify" recovery against an intact filesystem.
                 */
                if let Err(e) = damage() {
                    damage_err = Some(anyhow!("dev {dev}: {e}"));
                    return ControlFlow::Break(());
                }
                damaged += 1;
            }

            ControlFlow::Break(())
        }).map_err(|e| anyhow!("error walking btree nodes: {}", e))?;

        if !found {
            bail!("node at specified index not found");
        }

        if let Some(e) = damage_err {
            return Err(e);
        }

        if damaged == 0 {
            bail!("{} l={} idx found, but no replica was damaged - no pointer matched --dev {}",
                  kill.btree, kill.level, dev_idx);
        }
    }

    Ok(())
}

pub const CMD: super::CmdDef = typed_cmd!("kill_btree_node", "Remove a btree node", KillBtreeNodeCli, cmd_kill_btree_node);
