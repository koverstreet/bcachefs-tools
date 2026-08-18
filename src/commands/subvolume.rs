use std::{collections::HashMap, ffi::CStr, mem, os::fd::OwnedFd, path::{Path, PathBuf}};
use chrono::{Local, TimeZone};

use anyhow::{Context, Result};
use bch_bindgen::c::{
    BCH_SUBVOL_SNAPSHOT_RO, bch_ioctl_snapshot_node, bch_ioctl_snapshot_node_v2,
    bch_ioctl_snapshot_tree_query, bch_ioctl_snapshot_tree_query_v2,
    bch_ioctl_subvol_dirent, bch_ioctl_subvol_readdir, bch_ioctl_subvol_to_path,
};
use clap::{Parser, Subcommand, ValueEnum};

use crate::util::{fmt_sectors_human, fmt_bytes_human, fmt_num_human, open_dir, subvol_root};
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::ioctl::{ioctl_ptr, ioctl_rw, Ioctl, IoctlBuf,
    BCH_IOCTL_SNAPSHOT_TREE, BCH_IOCTL_SNAPSHOT_TREE_v2,
    BCH_IOCTL_SUBVOLUME_LIST, BCH_IOCTL_SUBVOLUME_TO_PATH};

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
    #[command(visible_aliases = ["new"],
        long_about = "Creates a new subvolume at the given path. Subvolumes are \
independently mountable filesystem trees, each with their own inode \
number space. Subvolume roots may be renamed or moved as subvolume \
roots, but ordinary files and directories cannot be renamed across \
subvolume boundaries.")]
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
    #[command(allow_missing_positional = true, visible_aliases = ["snap"],
        long_about = "Creates an instant, COW snapshot of a subvolume. Snapshots \
initially share all data with the source and only consume additional \
space as either diverges. Snapshots are writable by default; use \
--read-only for a frozen point-in-time copy.")]
    Snapshot {
        /// Make snapshot writable (the default)
        #[arg(long, conflicts_with = "read_only")]
        rw: bool,

        /// Make snapshot read only
        #[arg(long, short)]
        read_only: bool,
        source:    Option<PathBuf>,
        dest:      PathBuf,
    },

    /// List subvolumes
    #[command(visible_aliases = ["ls"],
        long_about = "Lists subvolumes in a bcachefs filesystem. Output \
includes path, subvolume ID, creation time, flags (ro, unlinked), and \
cumulative disk usage. Cumulative size is the total space that would be \
freed if the subvolume and all its snapshots were deleted.\n\n\
Use --tree for a hierarchical view, --recursive (-R) to include nested \
subvolumes, --snapshots to include snapshot subvolumes, or --json for \
machine-readable output. Sort by name, size, or creation time with \
--sort.")]
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

	/// Directory in a mounted filesystem
        target: PathBuf,
    },

    /// List snapshots and their disk usage
    #[command(visible_aliases = ["ls-snap", "list-snap"],
        long_about = "Lists snapshots with disk usage attribution. The \
default tree view shows the snapshot hierarchy with each node's own \
disk usage---space consumed exclusively by that snapshot, not shared \
with ancestors. Use --flat for a tabular view showing both own and \
cumulative (total) usage per snapshot. Cumulative usage is the sum of \
a snapshot's own usage plus all ancestors back to the root, \
representing the total space that would be freed if deleted.\n\n\
Use --json for machine-readable output including snapshot IDs, parent \
relationships, and sector counts. Use --recursive (-R) to list snapshot \
trees for nested subvolumes too.")]
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

        /// List snapshots for nested subvolumes too
        #[arg(long, short = 'R')]
        recursive: bool,

	/// Directory in a mounted filesystem
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

type SnapshotNode = bch_ioctl_snapshot_node_v2;

struct SnapshotTreeResult {
    master_subvol:  u32,
    root_snapshot:  u32,
    nodes:          Vec<SnapshotNode>,
}

// ---- Ioctl layer ----

const BCH_SUBVOLUME_RO:       u32 = 1 << 0;
const BCH_SUBVOLUME_UNLINKED: u32 = 1 << 2;

/// A header-plus-flexible-array ioctl with capacity/total retry protocol:
/// nr in is capacity, ERANGE reports the required total.
trait FlexArrayIoctl: Sized {
    type Node: Copy;
    type Ioc: Ioctl<Arg = Self>;
    fn set_capacity(&mut self, n: u32);
    fn nr(&self) -> u32;
    fn total(&self) -> u32;
    /// The trailing array, through the struct's flexible array member.
    /// Caller guarantees `nr` entries exist past the header.
    unsafe fn nodes(&self, nr: usize) -> &[Self::Node];
}

fn bcachefs_flex_ioctl<H: FlexArrayIoctl>(
    fd: &OwnedFd,
    mut arg: H,
) -> Result<(H, Vec<H::Node>)> {
    let mut capacity = 256u32;

    loop {
        arg.set_capacity(capacity);
        let mut buf = IoctlBuf::<H>::new::<H::Node>(capacity as usize);
        unsafe { std::ptr::copy_nonoverlapping(&arg, buf.as_mut_ptr(), 1) };

        match unsafe { ioctl_ptr::<H::Ioc>(fd, buf.as_mut_ptr()) } {
            Ok(_) => {}
            Err(e) if e.raw_os_error() == Some(libc::ERANGE) => {
                capacity = buf.hdr().total();
                continue;
            }
            Err(e) => return Err(e.into()),
        }

        let nr = (buf.hdr().nr() as usize).min(capacity as usize);
        let nodes = unsafe { buf.hdr().nodes(nr) }.to_vec();
        return Ok((unsafe { std::ptr::read(buf.hdr()) }, nodes));
    }
}

impl FlexArrayIoctl for bch_ioctl_snapshot_tree_query {
    type Node = bch_ioctl_snapshot_node;
    type Ioc = BCH_IOCTL_SNAPSHOT_TREE;
    fn set_capacity(&mut self, n: u32) { self.nr = n; }
    fn nr(&self) -> u32 { self.nr }
    fn total(&self) -> u32 { self.total }
    unsafe fn nodes(&self, nr: usize) -> &[Self::Node] { self.nodes.as_slice(nr) }
}

impl FlexArrayIoctl for bch_ioctl_snapshot_tree_query_v2 {
    type Node = bch_ioctl_snapshot_node_v2;
    type Ioc = BCH_IOCTL_SNAPSHOT_TREE_v2;
    fn set_capacity(&mut self, n: u32) { self.nr = n; }
    fn nr(&self) -> u32 { self.nr }
    fn total(&self) -> u32 { self.total }
    unsafe fn nodes(&self, nr: usize) -> &[Self::Node] { self.nodes.as_slice(nr) }
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

    ioctl_rw::<BCH_IOCTL_SUBVOLUME_LIST>(fd, &mut arg)
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
    let mut arg = bch_ioctl_subvol_to_path {
        subvolid,
        buf_size: buf.len() as u32,
        buf: buf.as_mut_ptr() as u64,
    };

    ioctl_rw::<BCH_IOCTL_SUBVOLUME_TO_PATH>(fd, &mut arg)
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
    match bcachefs_flex_ioctl(fd, bch_ioctl_snapshot_tree_query_v2 {
        tree_id,
        node_size: mem::size_of::<bch_ioctl_snapshot_node_v2>() as u32,
        ..Default::default()
    }) {
        Ok((hdr, nodes)) => Ok(SnapshotTreeResult {
            master_subvol: hdr.master_subvol,
            root_snapshot: hdr.root_snapshot,
            nodes,
        }),
        /* Kernel predates v2: fall back, without the key counters */
        Err(e) if e.downcast_ref::<std::io::Error>()
            .and_then(|e| e.raw_os_error()) == Some(libc::ENOTTY) => {
            let (hdr, v1) = bcachefs_flex_ioctl(fd, bch_ioctl_snapshot_tree_query {
                tree_id,
                ..Default::default()
            })?;

            Ok(SnapshotTreeResult {
                master_subvol: hdr.master_subvol,
                root_snapshot: hdr.root_snapshot,
                nodes: v1.iter().map(|n| bch_ioctl_snapshot_node_v2 {
                    id:       n.id,
                    parent:   n.parent,
                    children: n.children,
                    subvol:   n.subvol,
                    flags:    n.flags,
                    sectors:  n.sectors,
                    ..Default::default()
                }).collect(),
            })
        }
        Err(e) => Err(e),
    }
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
            SortBy::Time => entries.sort_by_key(|b| std::cmp::Reverse(b.1.otime_sec)),
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
    let mut label = format!("{} [{}, {} keys]", name,
        fmt_sectors_human(node.sectors), fmt_num_human(node.nr_keys));
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
            SortBy::Size => entries.sort_by_key(|b| std::cmp::Reverse(b.2)),
            SortBy::Time => {}
        }
    }

    println!("{:<24} {:<8} {:<12} {:<10} {:<8} {:<12} Flags",
        "Path", "ID", "Own", "Meta", "Keys", "Total");

    for (path, n, cumulative) in &entries {
        let f = flags_str(n.flags);
        let flags_display = if f.is_empty() { "-".to_string() } else { f };
        println!("{:<24} {:<8} {:<12} {:<10} {:<8} {:<12} {}",
            path, n.subvol,
            fmt_sectors_human(n.sectors),
            fmt_bytes_human(n.key_bytes),
            fmt_num_human(n.nr_keys),
            fmt_sectors_human(*cumulative),
            flags_display);
    }

    Ok(())
}

fn snapshot_json_value(dir: &Path) -> Result<serde_json::Value> {
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
            "nr_keys":  n.nr_keys,
            "key_bytes": n.key_bytes,
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

    Ok(serde_json::json!({
        "query_root": query_root,
        "nodes":      nodes_json,
    }))
}

fn print_snapshot_json(dir: &Path) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&snapshot_json_value(dir)?)?);
    Ok(())
}

fn collect_snapshot_targets(dir: &Path, recursive: bool) -> Result<Vec<(String, PathBuf)>> {
    let mut targets = vec![(dir.display().to_string(), dir.to_path_buf())];

    if recursive {
        for (path, entry) in collect_entries(dir, "", true)? {
            if entry.snapshot_parent == 0 {
                targets.push((path.clone(), dir.join(path)));
            }
        }
    }

    Ok(targets)
}

// ---- Command handlers ----

fn subvolume(cli: Cli) -> Result<()> {

    match cli.subcommands {
        Subcommands::Create { targets }                                         => cmd_create(targets),
        Subcommands::Delete { targets }                                         => cmd_delete(targets),
        /* rw is the default, so it only has to not contradict --read-only -
         * which clap enforces, so there's nothing left for it to say here: */
        Subcommands::Snapshot { read_only, source, dest, rw: _ }                => cmd_snapshot(read_only, source, dest),
        Subcommands::List { json, tree, recursive, snapshots, readonly, sort, target }
                                                                                => cmd_list(json, tree, recursive, snapshots, readonly, sort, target),
        Subcommands::ListSnapshots { flat, json, readonly, sort, recursive, target }
                                                                                => cmd_list_snapshots(flat, json, readonly, sort, recursive, target),
    }
}

/// Open a handle on the filesystem @path is in, or will be in.
///
/// The handle only selects which filesystem to send the ioctl to: the
/// subvolume ioctls carry their own dirfd and path and resolve it themselves.
/// So any file in the right filesystem will do - and for a path that doesn't
/// exist yet, that's the nearest ancestor that does. Walking too far - past a
/// mountpoint, onto a different bcachefs - is caught by the kernel, which
/// checks the resolved path against the filesystem the ioctl came in on and
/// returns EXDEV.
///
/// A relative path's last ancestor is "", which never exists: that's the cwd.
fn fs_for_path(path: &Path) -> Result<BcachefsHandle> {
    let dir = path
        .ancestors()
        .find(|p| p.exists())
        .unwrap_or(Path::new("."));

    BcachefsHandle::open(dir)
        .with_context(|| format!("Failed to open the filesystem at {}", dir.display()))
}

fn cmd_create(targets: Vec<PathBuf>) -> Result<()> {
    for target in targets {
        fs_for_path(&target)?
            .create_subvolume(target)
            .context("Failed to create the subvolume")?;
    }
    Ok(())
}

fn cmd_delete(targets: Vec<PathBuf>) -> Result<()> {
    for target in targets {
        let target = target
            .canonicalize()
            .context("subvolume path does not exist or can not be canonicalized")?;

        fs_for_path(&target)?
            .delete_subvolume(target)
            .context("Failed to delete the subvolume")?;
    }
    Ok(())
}

fn cmd_snapshot(read_only: bool, source: Option<PathBuf>, dest: PathBuf) -> Result<()> {
    fs_for_path(&dest)?
        .snapshot_subvolume(
            if read_only { BCH_SUBVOL_SNAPSHOT_RO } else { 0x0 },
            source,
            dest,
        )
        .context("Failed to snapshot the subvolume")?;
    Ok(())
}

fn cmd_list(json: bool, tree: bool, recursive: bool, snapshots: bool,
            readonly: bool, sort: Option<SortBy>, target: PathBuf) -> Result<()> {
    let recursive = recursive || tree;
    /* Listed paths are relative to the subvolume root, so that's where the
     * walk has to start - see subvol_root(). */
    let target = subvol_root(&target)?;
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
                      sort: Option<SortBy>, recursive: bool, target: PathBuf) -> Result<()> {
    /* The snapshot tree reported is the subvolume's, and a recursive walk
     * starts from its root either way - so name it by that root rather than
     * by whichever directory in it the caller happened to be standing in. */
    let target = subvol_root(&target)?;
    let targets = collect_snapshot_targets(&target, recursive)?;

    if json && recursive {
        let mut result = Vec::new();
        for (path, dir) in &targets {
            result.push(serde_json::json!({
                "path": path,
                "snapshots": snapshot_json_value(dir)?,
            }));
        }
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else if json {
        print_snapshot_json(&target)?;
    } else if flat {
        for (i, (path, dir)) in targets.iter().enumerate() {
            if recursive {
                if i != 0 {
                    println!();
                }
                println!("{}:", path);
            }
            print_snapshot_flat(dir, readonly, sort.clone())?;
        }
    } else {
        for (i, (path, dir)) in targets.iter().enumerate() {
            if recursive {
                if i != 0 {
                    println!();
                }
                println!("{}:", path);
            }
            print_snapshot_tree(dir)?;
        }
    }
    Ok(())
}

pub const CMD: super::CmdDef = typed_cmd!("subvolume", "Manage subvolumes and snapshots", aliases: ["subvol"], Cli, subvolume);
