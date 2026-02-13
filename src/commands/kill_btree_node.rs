use std::ops::ControlFlow;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Result};
use bch_bindgen::bcachefs;
use bch_bindgen::bkey::BkeySC;
use bch_bindgen::btree::{BtreeIterFlags, BtreeNodeIter, BtreeTrans};
use bch_bindgen::c;
use bch_bindgen::data::extents::bkey_ptrs;
use bch_bindgen::fs::Fs;
use bch_bindgen::opt_set;
use clap::Parser;

extern "C" {
    fn rust_dev_tryget_noerror(c: *mut c::bch_fs, dev: u32) -> *mut c::bch_dev;
    fn rust_dev_put(ca: *mut c::bch_dev);
}

/// RAII guard for a device reference obtained via bch2_dev_tryget_noerror.
struct DevRef(*mut c::bch_dev);

impl DevRef {
    fn get(fs: *mut c::bch_fs, dev: u32) -> Option<DevRef> {
        let ca = unsafe { rust_dev_tryget_noerror(fs, dev) };
        if ca.is_null() { None } else { Some(DevRef(ca)) }
    }
}

impl std::ops::Deref for DevRef {
    type Target = c::bch_dev;
    fn deref(&self) -> &c::bch_dev {
        unsafe { &*self.0 }
    }
}

impl Drop for DevRef {
    fn drop(&mut self) {
        unsafe { rust_dev_put(self.0) };
    }
}

struct KillNode {
    btree:  c::btree_id,
    level:  u32,
    idx:    u64,
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

    Ok(KillNode { btree, level, idx })
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

    let fs = Fs::open(&cli.devices, fs_opts)?;

    let block_size = unsafe { (*fs.raw).opts.block_size } as usize;
    let dev_idx = cli.dev.unwrap_or(-1);

    // O_DIRECT requires aligned buffers; bd_fd is opened with O_DIRECT
    let mut zeroes: *mut libc::c_void = std::ptr::null_mut();
    let r = unsafe { libc::posix_memalign(&mut zeroes, block_size, block_size) };
    if r != 0 {
        bail!("posix_memalign failed: {}", std::io::Error::from_raw_os_error(r));
    }
    unsafe { std::ptr::write_bytes(zeroes as *mut u8, 0, block_size) };

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

                let Some(ca) = DevRef::get(fs.raw, dev) else {
                    continue;
                };

                eprintln!("killing btree node on dev {} {} l={}\n  {}",
                    dev, kill.btree, kill.level, k.to_text(&fs));

                let fd = unsafe { (*ca.disk_sb.bdev).bd_fd };
                let offset = (ptr.offset() as i64) << 9;
                let ret = unsafe {
                    libc::pwrite(fd, zeroes, block_size, offset)
                };
                if ret as usize != block_size {
                    eprintln!("pwrite error: expected {} got {} {}",
                        block_size, ret, std::io::Error::last_os_error());
                }
            }

            ControlFlow::Break(())
        }).map_err(|e| anyhow!("error walking btree nodes: {}", e))?;

        if !found {
            unsafe { libc::free(zeroes) };
            bail!("node at specified index not found");
        }
    }

    unsafe { libc::free(zeroes) };
    Ok(())
}
