use std::path::PathBuf;

use anyhow::{anyhow, bail, Result};
use bch_bindgen::bcachefs;
use bch_bindgen::c;
use bch_bindgen::fs::Fs;
use bch_bindgen::opt_set;
use clap::Parser;

#[repr(C)]
struct KillNode {
    btree:  u32,
    level:  u32,
    idx:    u64,
}

extern "C" {
    fn rust_kill_btree_nodes(
        c: *mut c::bch_fs,
        nodes: *mut KillNode,
        nr_nodes: usize,
        dev_idx: i32,
    ) -> i32;
}

/// Make btree nodes unreadable (debugging tool)
#[derive(Parser, Debug)]
#[command(about = "Kill a specific btree node (debugging)")]
pub struct KillBtreeNodeCli {
    /// Node to kill (btree:level:idx)
    #[arg(short, long = "node")]
    nodes: Vec<String>,

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

    Ok(KillNode {
        btree: btree.into(),
        level,
        idx,
    })
}

pub fn cmd_kill_btree_node(argv: Vec<String>) -> Result<()> {
    let cli = KillBtreeNodeCli::parse_from(argv);

    if cli.nodes.is_empty() {
        bail!("no nodes specified (use -n btree:level:idx)");
    }

    let mut kill_nodes: Vec<KillNode> = cli.nodes.iter()
        .map(|s| parse_kill_node(s))
        .collect::<Result<Vec<_>>>()?;

    let mut fs_opts = bcachefs::bch_opts::default();
    opt_set!(fs_opts, read_only, 1);

    let fs = Fs::open(
        &cli.devices,
        fs_opts,
    )?;

    let dev_idx = cli.dev.unwrap_or(-1);

    let ret = unsafe {
        rust_kill_btree_nodes(
            fs.raw,
            kill_nodes.as_mut_ptr(),
            kill_nodes.len(),
            dev_idx,
        )
    };

    if ret != 0 {
        bail!("kill_btree_node failed: {}", ret);
    }

    Ok(())
}
