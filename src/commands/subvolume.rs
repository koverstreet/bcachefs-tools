use std::{collections::HashMap, env, ffi::CStr, mem, os::fd::{AsRawFd, OwnedFd}, path::{Path, PathBuf}};
use chrono::{Local, TimeZone};

use anyhow::{Context, Result};
use bch_bindgen::c::{
    BCH_SUBVOL_SNAPSHOT_RO, bch_ioctl_snapshot_node, bch_ioctl_subvol_dirent,
    bch_ioctl_subvol_readdir,
};
use clap::{Parser, Subcommand, ValueEnum};

use crate::util::fmt_sectors_human;
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::ioctl::bch_ioc_wr;

// ---- CLI definitions ----

#[derive(Clone, Debug, ValueEnum)]
enum SortBy {
    Name,
    Size,
    Time,
}

#[derive(Parser, Debug)]
pub struct Cli {
    #[command(subcommand)]
    subcommands: Subcommands,
}

/// Subvolumes-related commands
#[derive(Subcommand, Debug)]
enum Subcommands {
    /// Create a new subvolume
    #[command(visible_aliases = ["new"])]
    Create {
        /// Paths
        #[arg(required = true)]
        targets: Vec<PathBuf>,
    },

    /// Delete an existing subvolume
    #[command(visible_aliases = ["del"])]
    Delete {
        /// Path
        #[arg(required = true)]
        targets: Vec<PathBuf>,
    },

    /// Create a snapshot of a subvolume
    #[command(allow_missing_positional = true, visible_aliases = ["snap"])]
    Snapshot {
        /// Make snapshot read only
        #[arg(long, short)]
        read_only: bool,
        source:    Option<PathBuf>,
        dest:      PathBuf,
    },

    /// List subvolumes
    #[command(visible_aliases = ["ls"])]
    List {
        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Show subvolume tree structure (implies -R)
        #[arg(long, short)]
        tree: bool,

        /// List subvolumes recursively
        #[arg(long, short = 'R')]
        recursive: bool,

        /// Include snapshot subvolumes
        #[arg(long, short)]
        snapshots: bool,

        /// Only show read-only subvolumes
        #[arg(long)]
        readonly: bool,

        /// Sort order
        #[arg(long, value_enum)]
        sort: Option<SortBy>,

        /// Filesystem (device, mountpoint, or UUID)
        target: PathBuf,
    },

    /// List snapshots and their disk usage
    #[command(visible_aliases = ["ls-snap", "list-snap"])]
    ListSnapshots {
        /// Show flat list instead of tree
        #[arg(long, short)]
        flat: bool,

        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Only show read-only snapshots (flat view only)
        #[arg(long)]
        readonly: bool,

        /// Sort order (flat view only)
        #[arg(long, value_enum)]
        sort: Option<SortBy>,

        /// Filesystem (device, mountpoint, or UUID)
        target: PathBuf,
    },
}

// ---- Data types ----

struct SubvolEntry {
    subvolid: u32,
    flags: u32,
    snapshot_parent: u32,
    otime_sec: i64,
    otime_nsec: u32,
    path: String,
}

type SnapshotNode = bch_ioctl_snapshot_node;

struct SnapshotTreeResult {
    master_subvol:  u32,
    root_snapshot:  u32,
    nodes:          Vec<SnapshotNode>,
}

// ---- Ioctl layer ----

const BCH_IOCTL_SUBVOLUME_LIST: u32 = 31;
const BCH_IOCTL_SUBVOLUME_TO_PATH: u32 = 32;
const BCH_IOCTL_SNAPSHOT_TREE_USAGE: u32 = 33;

const BCH_SUBVOLUME_RO:       u32 = 1 << 0;
const BCH_SUBVOLUME_UNLINKED: u32 = 1 << 2;

fn bcachefs_ioctl<T>(fd: &OwnedFd, nr: u32, arg: &mut T) -> std::io::Result<()> {
    let ret = unsafe { libc::ioctl(fd.as_raw_fd(), bch_ioc_wr::<T>(nr), arg as *mut T) };
    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

trait FlexArrayIoctl: Copy {
    type Node: Copy;
    const NR: u32;
    fn set_capacity(&mut self, n: u32);
    fn nr(&self) -> u32;
    fn total(&self) -> u32;
}

fn bcachefs_flex_ioctl<H: FlexArrayIoctl>(
    fd: &OwnedFd,
    mut arg: H,
) -> Result<(H, Vec<H::Node>)> {
    let hdr_size = mem::size_of::<H>();
    let node_size = mem::size_of::<H::Node>();
    let request = bch_ioc_wr::<H>(H::NR);
    let mut capacity = 256u32;

    loop {
        arg.set_capacity(capacity);
        let buf_size = hdr_size + node_size * capacity as usize;
        let mut buf = vec![0u8; buf_size];

        unsafe {
            std::ptr::copy_nonoverlapping(
                &arg as *const H as *const u8, buf.as_mut_ptr(), hdr_size);
        }

        let ret = unsafe { libc::ioctl(fd.as_raw_fd(), request, buf.as_mut_ptr()) };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ERANGE) {
                let hdr = unsafe { &*(buf.as_ptr() as *const H) };
                capacity = hdr.total();
                continue;
            }
            return Err(err.into());
        }

        let hdr = unsafe { *(buf.as_ptr() as *const H) };
        let nr = hdr.nr() as usize;
        let nodes = (0..nr).map(|i| unsafe {
            std::ptr::read_unaligned(
                buf.as_ptr().add(hdr_size + i * node_size) as *const H::Node)
        }).collect();

        return Ok((hdr, nodes));
    }
}

#[repr(C)]
struct BchIoctlSubvolToPath {
    subvolid:   u32,
    buf_size:   u32,
    buf:        u64,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct BchIoctlSnapshotTreeQuery {
    tree_id:        u32,
    master_subvol:  u32,
    root_snapshot:  u32,
    nr:             u32,
    total:          u32,
    pad:            u32,
}

impl FlexArrayIoctl for BchIoctlSnapshotTreeQuery {
    type Node = bch_ioctl_snapshot_node;
    const NR: u32 = BCH_IOCTL_SNAPSHOT_TREE_USAGE;
    fn set_capacity(&mut self, n: u32) { self.nr = n; }
    fn nr(&self) -> u32 { self.nr }
    fn total(&self) -> u32 { self.total }
}

fn open_dir(path: &Path) -> Result<OwnedFd> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(path)
        .with_context(|| format!("Failed to open {}", path.display()))?;
    Ok(f.into())
}

fn subvol_readdir(fd: &OwnedFd, pos: &mut u32) -> Result<Vec<SubvolEntry>> {
    let buf_size = 64 * 1024u32;
    let mut buf = vec![0u8; buf_size as usize];
    let mut arg = bch_ioctl_subvol_readdir {
        pos: *pos,
        buf_size,
        buf: buf.as_mut_ptr() as u64,
        used: 0,
        pad: 0,
    };

    bcachefs_ioctl(fd, BCH_IOCTL_SUBVOLUME_LIST, &mut arg)
        .context("BCH_IOCTL_SUBVOLUME_LIST")?;
    *pos = arg.pos;

    let mut entries = Vec::new();
    let mut offset = 0usize;
    let used = arg.used as usize;
    let hdr_size = mem::size_of::<bch_ioctl_subvol_dirent>();

    while offset + hdr_size <= used {
        let dirent = unsafe { &*(buf.as_ptr().add(offset) as *const bch_ioctl_subvol_dirent) };
        let reclen = dirent.reclen as usize;
        if reclen < hdr_size || offset + reclen > used { break; }

        let path_bytes = &buf[offset + hdr_size..offset + reclen];
        let path = CStr::from_bytes_until_nul(path_bytes)
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_default();

        entries.push(SubvolEntry {
            subvolid: dirent.subvolid,
            flags: dirent.flags,
            snapshot_parent: dirent.snapshot_parent,
            otime_sec: dirent.otime_sec as i64,
            otime_nsec: dirent.otime_nsec,
            path,
        });
        offset += reclen;
    }

    Ok(entries)
}

fn list_children(fd: &OwnedFd) -> Result<Vec<SubvolEntry>> {
    let mut all = Vec::new();
    let mut pos = 0u32;
    loop {
        let entries = subvol_readdir(fd, &mut pos)?;
        if entries.is_empty() { break; }
        all.extend(entries);
    }
    Ok(all)
}

fn subvol_to_path(fd: &OwnedFd, subvolid: u32) -> Result<String> {
    let mut buf = vec![0u8; 4096];
    let mut arg = BchIoctlSubvolToPath {
        subvolid,
        buf_size: buf.len() as u32,
        buf: buf.as_mut_ptr() as u64,
    };

    bcachefs_ioctl(fd, BCH_IOCTL_SUBVOLUME_TO_PATH, &mut arg)
        .context("BCH_IOCTL_SUBVOLUME_TO_PATH")?;

    let path = CStr::from_bytes_until_nul(&buf)
        .context("invalid path from ioctl")?
        .to_string_lossy()
        .into_owned();
    Ok(format!("/{}", path))
}

fn resolve_subvol_path(fd: &OwnedFd, subvolid: u32) -> Option<String> {
    subvol_to_path(fd, subvolid).ok()
}

fn query_snapshot_tree(fd: &OwnedFd, tree_id: u32) -> Result<SnapshotTreeResult> {
    let (hdr, nodes) = bcachefs_flex_ioctl(fd, BchIoctlSnapshotTreeQuery {
        tree_id,
        ..Default::default()
    })?;

    Ok(SnapshotTreeResult {
        master_subvol: hdr.master_subvol,
        root_snapshot: hdr.root_snapshot,
        nodes,
    })
}

fn compute_subvol_sizes(tree: &SnapshotTreeResult) -> HashMap<u32, u64> {
    let by_id: HashMap<u32, &SnapshotNode> = tree.nodes.iter()
        .map(|n| (n.id, n)).collect();

    let mut sizes = HashMap::new();
    for n in &tree.nodes {
        if n.subvol == 0 { continue; }
        let mut cumulative = 0u64;
        let mut cur = n.id;
        while let Some(node) = by_id.get(&cur) {
            cumulative += node.sectors;
            if node.parent == 0 { break; }
            cur = node.parent;
        }
        sizes.insert(n.subvol, cumulative);
    }
    sizes
}

fn subvol_sizes(fd: &OwnedFd) -> Option<HashMap<u32, u64>> {
    let tree = query_snapshot_tree(fd, 0).ok()?;
    Some(compute_subvol_sizes(&tree))
}

// ---- Formatting helpers ----

fn flags_str(flags: u32) -> String {
    let mut parts = Vec::new();
    if flags & BCH_SUBVOLUME_RO != 0       { parts.push("ro"); }
    if flags & BCH_SUBVOLUME_UNLINKED != 0 { parts.push("unlinked"); }
    if parts.is_empty() {
        String::new()
    } else {
        parts.join(",")
    }
}

fn format_time(sec: i64, _nsec: u32) -> String {
    if sec == 0 {
        return "-".to_string();
    }
    match Local.timestamp_opt(sec, 0) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        _ => sec.to_string(),
    }
}

fn snapshot_parent_str(fd: &OwnedFd, parent: u32) -> String {
    if parent == 0 {
        return String::new();
    }
    resolve_subvol_path(fd, parent)
        .unwrap_or_else(|| parent.to_string())
}

// ---- Display: subvolume list ----

fn collect_entries(dir: &Path, prefix: &str, recursive: bool) -> Result<Vec<(String, SubvolEntry)>> {
    let fd = open_dir(dir)?;
    let children = list_children(&fd)?;
    let mut out = Vec::new();

    for e in children {
        let full_path = if prefix.is_empty() {
            e.path.clone()
        } else {
            format!("{}/{}", prefix, e.path)
        };

        let child_dir = dir.join(&e.path);
        out.push((full_path.clone(), e));

        if recursive {
            if let Ok(sub) = collect_entries(&child_dir, &full_path, true) {
                out.extend(sub);
            }
        }
    }

    Ok(out)
}

fn print_flat(dir: &Path, recursive: bool, show_snapshots: bool,
              readonly: bool, sort: Option<SortBy>) -> Result<()> {
    let fd = open_dir(dir)?;
    let sizes = subvol_sizes(&fd);
    let mut entries = collect_entries(dir, "", recursive)?;

    entries.retain(|(_, e)| {
        if !show_snapshots && e.snapshot_parent != 0 { return false; }
        if readonly && (e.flags & BCH_SUBVOLUME_RO) == 0 { return false; }
        true
    });

    if let Some(ref sort) = sort {
        match sort {
            SortBy::Name => entries.sort_by(|a, b| a.0.cmp(&b.0)),
            SortBy::Size => entries.sort_by(|a, b| {
                let sa = sizes.as_ref().and_then(|s| s.get(&a.1.subvolid)).copied().unwrap_or(0);
                let sb = sizes.as_ref().and_then(|s| s.get(&b.1.subvolid)).copied().unwrap_or(0);
                sb.cmp(&sa)
            }),
            SortBy::Time => entries.sort_by(|a, b| b.1.otime_sec.cmp(&a.1.otime_sec)),
        }
    }

    if show_snapshots {
        println!("{:<24} {:<8} {:<16} {:<12} {:<12} Snapshot",
            "Path", "ID", "Created", "Flags", "Size");
    } else {
        println!("{:<24} {:<8} {:<16} {:<12} Size",
            "Path", "ID", "Created", "Flags");
    }

    for (path, e) in &entries {
        let f = flags_str(e.flags);
        let flags_display = if f.is_empty() { "-".to_string() } else { f };
        let size = sizes.as_ref()
            .and_then(|s| s.get(&e.subvolid))
            .map(|&s| fmt_sectors_human(s))
            .unwrap_or_default();

        if show_snapshots {
            let snap = if e.snapshot_parent != 0 {
                snapshot_parent_str(&fd, e.snapshot_parent)
            } else {
                String::new()
            };
            println!("{:<24} {:<8} {:<16} {:<12} {:<12} {}",
                path, e.subvolid,
                format_time(e.otime_sec, e.otime_nsec),
                flags_display, size, snap);
        } else {
            println!("{:<24} {:<8} {:<16} {:<12} {}",
                path, e.subvolid,
                format_time(e.otime_sec, e.otime_nsec),
                flags_display, size);
        }
    }

    Ok(())
}

fn print_tree_recursive(dir: &Path, prefix: &str, show_snapshots: bool,
                        sizes: &Option<HashMap<u32, u64>>) -> Result<()> {
    let fd = open_dir(dir)?;
    let entries = list_children(&fd)?;

    let entries: Vec<_> = entries.into_iter()
        .filter(|e| show_snapshots || e.snapshot_parent == 0)
        .collect();

    for (i, e) in entries.iter().enumerate() {
        let is_last = i == entries.len() - 1;
        let connector = if is_last { "└── " } else { "├── " };
        let child_indent = if is_last { "    " } else { "│   " };

        let mut annotations = Vec::new();
        if e.snapshot_parent != 0 {
            let parent = resolve_subvol_path(&fd, e.snapshot_parent)
                .unwrap_or_else(|| e.snapshot_parent.to_string());
            annotations.push(format!("snap of {}", parent));
        }
        let f = flags_str(e.flags);
        if !f.is_empty() { annotations.push(f); }
        if let Some(&sectors) = sizes.as_ref().and_then(|s| s.get(&e.subvolid)) {
            annotations.push(fmt_sectors_human(sectors));
        }
        let otime = format_time(e.otime_sec, e.otime_nsec);
        if otime != "-" { annotations.push(otime); }
        let suffix = if annotations.is_empty() {
            String::new()
        } else {
            format!(" [{}]", annotations.join(", "))
        };

        println!("{}{}{}{}", prefix, connector, e.path, suffix);

        let next_prefix = format!("{}{}", prefix, child_indent);
        print_tree_recursive(&dir.join(&e.path), &next_prefix, show_snapshots, sizes)?;
    }

    Ok(())
}

fn collect_subvol_json(dir: &Path, recursive: bool, show_snapshots: bool, readonly: bool,
                       sizes: &Option<HashMap<u32, u64>>) -> Result<Vec<serde_json::Value>> {
    let fd = open_dir(dir)?;
    let entries = list_children(&fd)?;
    let mut result = Vec::new();

    for e in &entries {
        if !show_snapshots && e.snapshot_parent != 0 { continue; }
        if readonly && (e.flags & BCH_SUBVOLUME_RO) == 0 { continue; }

        let mut obj = serde_json::json!({
            "subvolid": e.subvolid,
            "path":     &e.path,
        });
        if e.otime_sec != 0 {
            obj["otime"] = serde_json::json!(format_time(e.otime_sec, e.otime_nsec));
            obj["otime_unix"] = serde_json::json!(e.otime_sec);
        }
        if e.snapshot_parent != 0 {
            let parent_path = resolve_subvol_path(&fd, e.snapshot_parent)
                .unwrap_or_else(|| e.snapshot_parent.to_string());
            obj["snapshot_parent"] = serde_json::json!(parent_path);
        }
        let f = flags_str(e.flags);
        if !f.is_empty() {
            obj["flags"] = serde_json::json!(f);
        }
        if let Some(&sectors) = sizes.as_ref().and_then(|s| s.get(&e.subvolid)) {
            obj["size"] = serde_json::json!(fmt_sectors_human(sectors));
            obj["sectors"] = serde_json::json!(sectors);
        }
        if recursive {
            let children = collect_subvol_json(&dir.join(&e.path), true, show_snapshots, readonly, sizes)?;
            if !children.is_empty() {
                obj["children"] = serde_json::json!(children);
            }
        }
        result.push(obj);
    }

    Ok(result)
}

fn print_json(dir: &Path, recursive: bool, show_snapshots: bool, readonly: bool) -> Result<()> {
    let fd = open_dir(dir)?;
    let sizes = subvol_sizes(&fd);
    let tree = collect_subvol_json(dir, recursive, show_snapshots, readonly, &sizes)?;
    println!("{}", serde_json::to_string_pretty(&tree)?);
    Ok(())
}

// ---- Display: snapshot tree ----

fn snapshot_node_children(node: &SnapshotNode, by_id: &HashMap<u32, &SnapshotNode>) -> Vec<u32> {
    node.children.iter()
        .copied()
        .filter(|&c| c != 0 && by_id.contains_key(&c))
        .collect()
}

fn snapshot_node_label(id: u32, node: &SnapshotNode, names: &HashMap<u32, String>) -> String {
    let name = names.get(&id)
        .cloned()
        .unwrap_or_else(|| "(shared)".to_string());
    let mut label = format!("{} [{}]", name, fmt_sectors_human(node.sectors));
    let f = flags_str(node.flags);
    if !f.is_empty() {
        label.push_str(&format!(" ({})", f));
    }
    label
}

fn print_snapshot_subtree(
    id: u32,
    by_id: &HashMap<u32, &SnapshotNode>,
    names: &HashMap<u32, String>,
    prefix: &str,
    is_last: bool,
) {
    let Some(node) = by_id.get(&id) else { return };

    let connector = if is_last { "└── " } else { "├── " };
    println!("{}{}{}", prefix, connector, snapshot_node_label(id, node, names));

    let child_prefix = if is_last {
        format!("{}    ", prefix)
    } else {
        format!("{}│   ", prefix)
    };

    let children = snapshot_node_children(node, by_id);
    for (i, &child_id) in children.iter().enumerate() {
        print_snapshot_subtree(child_id, by_id, names, &child_prefix, i == children.len() - 1);
    }
}

fn print_snapshot_tree(dir: &Path) -> Result<()> {
    let fd = open_dir(dir)?;
    let tree = match query_snapshot_tree(&fd, 0) {
        Ok(t) => t,
        Err(e) => {
            if let Some(inner) = e.downcast_ref::<std::io::Error>() {
                if inner.raw_os_error() == Some(libc::ENOTTY) {
                    eprintln!("snapshot tree ioctl not supported by this kernel");
                    return Ok(());
                }
            }
            return Err(e);
        }
    };

    if tree.nodes.is_empty() {
        println!("(no snapshot nodes)");
        return Ok(());
    }

    let by_id: HashMap<u32, &SnapshotNode> = tree.nodes.iter().map(|n| (n.id, n)).collect();

    let mut names: HashMap<u32, String> = HashMap::new();
    for n in &tree.nodes {
        if n.subvol != 0 {
            let name = resolve_subvol_path(&fd, n.subvol)
                .unwrap_or_else(|| format!("subvol {}", n.subvol));
            names.insert(n.id, name);
        }
    }

    if let Some(root) = by_id.get(&tree.root_snapshot) {
        println!("{}", snapshot_node_label(tree.root_snapshot, root, &names));

        let children = snapshot_node_children(root, &by_id);
        for (i, &child_id) in children.iter().enumerate() {
            print_snapshot_subtree(child_id, &by_id, &names, "", i == children.len() - 1);
        }
    }

    Ok(())
}

fn print_snapshot_flat(dir: &Path, readonly: bool, sort: Option<SortBy>) -> Result<()> {
    let fd = open_dir(dir)?;
    let tree = query_snapshot_tree(&fd, 0)?;
    let sizes = compute_subvol_sizes(&tree);

    let mut entries: Vec<(String, &SnapshotNode, u64)> = tree.nodes.iter()
        .filter(|n| n.subvol != 0)
        .filter(|n| !readonly || (n.flags & BCH_SUBVOLUME_RO) != 0)
        .map(|n| {
            let path = resolve_subvol_path(&fd, n.subvol)
                .unwrap_or_else(|| format!("subvol {}", n.subvol));
            let cumulative = sizes.get(&n.subvol).copied().unwrap_or(0);
            (path, n, cumulative)
        })
        .collect();

    if let Some(ref sort) = sort {
        match sort {
            SortBy::Name => entries.sort_by(|a, b| a.0.cmp(&b.0)),
            SortBy::Size => entries.sort_by(|a, b| b.2.cmp(&a.2)),
            SortBy::Time => {}
        }
    }

    println!("{:<24} {:<8} {:<12} {:<12} Flags",
        "Path", "ID", "Own", "Total");

    for (path, n, cumulative) in &entries {
        let f = flags_str(n.flags);
        let flags_display = if f.is_empty() { "-".to_string() } else { f };
        println!("{:<24} {:<8} {:<12} {:<12} {}",
            path, n.subvol,
            fmt_sectors_human(n.sectors),
            fmt_sectors_human(*cumulative),
            flags_display);
    }

    Ok(())
}

fn print_snapshot_json(dir: &Path) -> Result<()> {
    let fd = open_dir(dir)?;
    let tree = query_snapshot_tree(&fd, 0)?;

    let mut nodes_json = Vec::new();
    for n in &tree.nodes {
        let mut obj = serde_json::json!({
            "id":       n.id,
            "parent":   n.parent,
            "children": n.children.iter().filter(|&&c| c != 0).collect::<Vec<_>>(),
            "subvol":   n.subvol,
            "sectors":  n.sectors,
            "size":     fmt_sectors_human(n.sectors),
        });
        let f = flags_str(n.flags);
        if !f.is_empty() {
            obj["flags"] = serde_json::json!(f);
        }
        if n.subvol != 0 {
            if let Some(path) = resolve_subvol_path(&fd, n.subvol) {
                obj["path"] = serde_json::json!(path);
            }
        }
        nodes_json.push(obj);
    }

    let mut query_root = serde_json::json!({
        "subvol":   tree.master_subvol,
        "snapshot": tree.root_snapshot,
    });
    if let Some(path) = resolve_subvol_path(&fd, tree.master_subvol) {
        query_root["path"] = path.into();
    }

    println!("{}", serde_json::to_string_pretty(&serde_json::json!({
        "query_root": query_root,
        "nodes":      nodes_json,
    }))?);
    Ok(())
}

// ---- Command handlers ----

pub fn subvolume(argv: Vec<String>) -> Result<()> {
    let cli = Cli::parse_from(argv);

    match cli.subcommands {
        Subcommands::Create { targets }                                         => cmd_create(targets),
        Subcommands::Delete { targets }                                         => cmd_delete(targets),
        Subcommands::Snapshot { read_only, source, dest }                       => cmd_snapshot(read_only, source, dest),
        Subcommands::List { json, tree, recursive, snapshots, readonly, sort, target }
                                                                                => cmd_list(json, tree, recursive, snapshots, readonly, sort, target),
        Subcommands::ListSnapshots { flat, json, readonly, sort, target }       => cmd_list_snapshots(flat, json, readonly, sort, target),
    }
}

fn cmd_create(targets: Vec<PathBuf>) -> Result<()> {
    for target in targets {
        let target = if target.is_absolute() {
            target
        } else {
            env::current_dir()
                .map(|p| p.join(target))
                .context("unable to get current directory")?
        };

        if let Some(dirname) = target.parent() {
            let fs = BcachefsHandle::open(dirname).context("Failed to open the filesystem")?;
            fs.create_subvolume(target)
                .context("Failed to create the subvolume")?;
        }
    }
    Ok(())
}

fn cmd_delete(targets: Vec<PathBuf>) -> Result<()> {
    for target in targets {
        let target = target
            .canonicalize()
            .context("subvolume path does not exist or can not be canonicalized")?;

        if let Some(dirname) = target.parent() {
            let fs = BcachefsHandle::open(dirname).context("Failed to open the filesystem")?;
            fs.delete_subvolume(target)
                .context("Failed to delete the subvolume")?;
        }
    }
    Ok(())
}

fn cmd_snapshot(read_only: bool, source: Option<PathBuf>, dest: PathBuf) -> Result<()> {
    if let Some(dirname) = dest.parent() {
        let dot = PathBuf::from(".");
        let dir = if dirname.as_os_str().is_empty() { &dot } else { dirname };
        let fs = BcachefsHandle::open(dir).context("Failed to open the filesystem")?;

        fs.snapshot_subvolume(
            if read_only { BCH_SUBVOL_SNAPSHOT_RO } else { 0x0 },
            source,
            dest,
        )
        .context("Failed to snapshot the subvolume")?;
    }
    Ok(())
}

fn cmd_list(json: bool, tree: bool, recursive: bool, snapshots: bool,
            readonly: bool, sort: Option<SortBy>, target: PathBuf) -> Result<()> {
    let recursive = recursive || tree;
    if json {
        print_json(&target, recursive, snapshots, readonly)?;
    } else if tree {
        let fd = open_dir(&target)?;
        let sizes = subvol_sizes(&fd);
        println!("{}", target.display());
        print_tree_recursive(&target, "", snapshots, &sizes)?;
    } else {
        print_flat(&target, recursive, snapshots, readonly, sort)?;
    }
    Ok(())
}

fn cmd_list_snapshots(flat: bool, json: bool, readonly: bool,
                      sort: Option<SortBy>, target: PathBuf) -> Result<()> {
    if json {
        print_snapshot_json(&target)?;
    } else if flat {
        print_snapshot_flat(&target, readonly, sort)?;
    } else {
        print_snapshot_tree(&target)?;
    }
    Ok(())
}
