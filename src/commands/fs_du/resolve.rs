//! Inode number and snapshot -> path, in userspace: each inode names its
//! directory and its dirent's position there (bi_dir, bi_dir_offset), walked up
//! to the root as the kernel's inum_to_path() walks it. A subvolume's root is
//! named in its parent subvolume, bi_parent_subvol, whose snapshot the dirent
//! is read in.
//!
//! Visibility is decided here, not by the ioctl: keys at a position are read
//! across all snapshots, whiteouts included, and the one in the nearest
//! snapshot at or above the one asked for is it; a whiteout there means
//! nothing is. BCH_IOCTL_QUERY_BTREE_KEYS has no snapshot filtering to rely
//! on: it reads through a node iterator, which sees every snapshot.
//!
//! Every path walked is memoized by (inode, snapshot), and with it a
//! failure: a walk stops at the first ancestor already known, so a tree is
//! read once whatever the number of files in it. A key read at a snapshot
//! shared by several subvolumes resolves to the path in the subvolume it
//! was written in.
//!
//! None means the inode isn't reachable: gone, or renamed or unlinked since
//! its data was sampled - a dirent that doesn't name it back isn't used - or
//! a walk that loops.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{anyhow, Result};
use bcachefs_kernel::btree::bkey::prelude::*;
use bcachefs_kernel::btree::bkey::{pos, spos, BkeySC, POS_MAX};
use bcachefs_kernel::c;
use bcachefs_kernel::dirent::Dirent;
use bcachefs_kernel::format::Btree;
use bcachefs_kernel::fs::Fs;
use bcachefs_kernel::inode;
use bcachefs_kernel::namei::dirent_points_to_inode;

use super::Subvols;
use crate::util::statx_subvol;
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::online_iter::{OnlineBtreeIter, OnlineIterFlags};

/// Deeper than any real tree: past it, a walk is a loop.
const MAX_DEPTH: usize = 4096;

/// Snapshot -> its parent, and each one's ancestry, nearest first.
struct Snapshots {
    parent:    HashMap<u32, u32>,
    ancestors: HashMap<u32, Rc<[u32]>>,
}

impl Snapshots {
    fn load(handle: &BcachefsHandle) -> Result<Self> {
        let mut parent = HashMap::new();
        let mut iter = OnlineBtreeIter::new(handle, Btree::snapshots, 0, pos(0, 0), POS_MAX,
                                            OnlineIterFlags::default());
        while let Some(k) = iter.next()? {
            if let Some(s) = k.arm::<c::Snapshot>() {
                parent.insert(k.k.p.offset as u32, s.v().parent.get());
            }
        }
        Ok(Snapshots { parent, ancestors: HashMap::new() })
    }

    /// @snapshot and its ancestors, nearest first.
    fn ancestors(&mut self, snapshot: u32) -> Rc<[u32]> {
        let parent = &self.parent;
        self.ancestors.entry(snapshot)
            .or_insert_with(|| {
                std::iter::successors(Some(snapshot), |s| parent.get(s).copied().filter(|&p| p != 0))
                    .take(MAX_DEPTH)
                    .collect()
            })
            .clone()
    }
}

pub(super) struct Resolver<'a> {
    handle:    &'a BcachefsHandle,
    fs:        &'a Fs,
    subvols:   Subvols,
    snapshots: Snapshots,
    paths:     HashMap<(u64, u32), Option<Rc<Path>>>,
}

impl<'a> Resolver<'a> {
    pub(super) fn new(handle: &'a BcachefsHandle, fs: &'a Fs) -> Result<Self> {
        Ok(Resolver {
            handle,
            fs,
            subvols:   Subvols::default(),
            snapshots: Snapshots::load(handle)?,
            paths:     HashMap::new(),
        })
    }

    pub(super) fn subvol_snapshot(&mut self, subvol: u64) -> Result<Option<u32>> {
        self.subvols.snapshot(self.handle, subvol)
    }

    /// What @f makes of the key visible in @snapshot at @inum:@offset of
    /// @btree, None if there's none or it's a whiteout.
    fn visible<T>(&mut self, btree: Btree, inum: u64, offset: u64, snapshot: u32,
                  mut f: impl FnMut(BkeySC<'_>) -> Option<T>) -> Result<Option<T>> {
        let ancestors = self.snapshots.ancestors(snapshot);
        let mut iter = OnlineBtreeIter::with_buf_size(self.handle, btree, 0,
            spos(inum, offset, 0), spos(inum, offset, u32::MAX),
            OnlineIterFlags::ALL_SNAPSHOTS | OnlineIterFlags::NOFILTER_WHITEOUTS, 4096);
        let mut best: Option<(usize, Option<T>)> = None;

        while let Some(k) = iter.next()? {
            let Some(rank) = ancestors.iter().position(|&s| s == k.k.p.snapshot) else { continue };
            if best.as_ref().is_none_or(|(r, _)| rank < *r) {
                best = Some((rank, f(k)));
            }
        }
        Ok(best.and_then(|(_, v)| v))
    }

    pub(super) fn inode(&mut self, inum: u64, snapshot: u32) -> Result<Option<c::bch_inode_unpacked>> {
        let fs = self.fs;
        self.visible(Btree::inodes, 0, inum, snapshot,
                     |k| inode::bkey_is_inode(k.k).then(|| inode::unpack(fs, k)))
    }

    /// The name @inode has in its directory, read in @snapshot.
    fn name(&mut self, inode: &c::bch_inode_unpacked, snapshot: u32) -> Result<Option<OsString>> {
        self.visible(Btree::dirents, inode.bi_dir, inode.bi_dir_offset, snapshot, |k| {
            Dirent::new(k)
                .filter(|d| dirent_points_to_inode(*d, inode))
                .map(|d| d.name().to_os_string())
        })
    }

    /// @inum's path, as seen in @snapshot.
    pub(super) fn path(&mut self, inum: u64, snapshot: u32) -> Result<Option<Rc<Path>>> {
        let mut walked: Vec<((u64, u32), Option<OsString>)> = Vec::new();
        let mut at = (inum, snapshot);

        let top = loop {
            if let Some(path) = self.paths.get(&at) {
                break path.clone();
            }
            if walked.len() == MAX_DEPTH {
                break None;
            }
            let Some(inode) = self.inode(at.0, at.1)? else { break None };

            if inode.bi_subvol == c::BCACHEFS_ROOT_SUBVOL && inode.bi_inum == u64::from(c::BCACHEFS_ROOT_INO) {
                walked.push((at, None));
                break Some(Rc::from(Path::new("/")));
            }
            if inode.bi_dir == 0 && inode.bi_dir_offset == 0 {
                break None;
            }

            let snapshot = match inode.bi_parent_subvol {
                0 => at.1,
                parent => match self.subvol_snapshot(parent.into())? {
                    Some(snapshot) => snapshot,
                    None => break None,
                },
            };
            let Some(name) = self.name(&inode, snapshot)? else { break None };
            walked.push((at, Some(name)));
            at = (inode.bi_dir, snapshot);
        };

        let mut path = top;
        for (at, name) in walked.into_iter().rev() {
            if let Some(name) = name {
                path = path.map(|p| Rc::from(p.join(name)));
            }
            self.paths.insert(at, path.clone());
        }
        Ok(path)
    }
}

/// The path @path has inside the filesystem, from its root.
pub(super) fn fs_path(r: &mut Resolver<'_>, path: &Path) -> Result<PathBuf> {
    let stx = statx_subvol(path, 0)?
        .ok_or_else(|| anyhow!("not on a bcachefs filesystem"))?;
    let snapshot = r.subvol_snapshot(stx.stx_subvol)?
        .ok_or_else(|| anyhow!("subvolume {} not found", stx.stx_subvol))?;
    r.path(stx.stx_ino, snapshot)?
        .map(|p| p.to_path_buf())
        .ok_or_else(|| anyhow!("inode {} not reachable from the root", stx.stx_ino))
}
