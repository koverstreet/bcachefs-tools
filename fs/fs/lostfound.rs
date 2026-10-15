// SPDX-License-Identifier: GPL-2.0

//! lost+found, and reattaching unreachable inodes in it.
//!
//! There's one lost+found per snapshot tree, in the tree's root snapshot, so
//! every branch inherits it. lookup_lostfound() finds it from a snapshot,
//! creating it if the tree has none or restoring its dirent where a snapshot
//! deleted it. reattach_inode() names an inode there - its number, or
//! "subvol-<n>" for a subvolume root - then points the descendant versions that
//! also need reattaching at the new dirent, and hides it from the rest.
//!
//! fsck-inject tests each path: creating vs restoring lost+found, adopting a
//! reattach dirent after a partial commit, a taken name, the descendant fixup.

use crate::btree::bkey::{pos, spos, BkeyI, BkeySC, POS_MIN};
use crate::btree::iter::{
    is_restart, BtreeIter, BtreeIterFlags, CommitFlags, TransAttempt, UpdateTriggerFlags,
};
use crate::c;
use crate::dirent::{self, Dirent, DirentTarget, Dirents};
use crate::errcode::{bch_errcode, BchError, Found};
use crate::inode;
use crate::namei;
use crate::snapshots::{snapshot, subvolume};
use crate::str_hash;
use crate::util::alloc::{flags::GFP_KERNEL, KVVec};
use crate::util::os_str::{OsStr, OsStrExt};
use crate::util::Printbuf;
use crate::{bch_err, bch_err_msg, bch_info, bch_notice, bch_verbose};
use core::mem::size_of;
use core::ops::ControlFlow;

const LOSTFOUND: &[u8] = b"lost+found";

const CREATE: (BtreeIterFlags, UpdateTriggerFlags) =
    (BtreeIterFlags::STR_HASH_MUST_CREATE, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE);

/// The inode lost+found names in @root at @snapshot. Not a directory is an
/// error, not None: we'd fail to create one over it.
fn lostfound_dirent(
    t:         &TransAttempt<'_, '_>,
    root_hash: &c::bch_hash_info,
    root:      c::subvol_inum,
    snapshot:  u32,
) -> Result<Option<u64>, BchError> {
    let fs = t.fs();
    let mut iter = BtreeIter::uninit();
    let Some(k) = str_hash::lookup_in_snapshot::<Dirents>(t, &mut iter, root_hash, root,
                                                           OsStr::from_bytes(LOSTFOUND),
                                                           BtreeIterFlags::empty(), snapshot)
        .found()? else { return Ok(None) };

    let d = Dirent::new(k).expect("a dirent");
    match d.target() {
        DirentTarget::Inode(inum) if d.d_type() as u32 == c::DT_DIR => Ok(Some(inum)),
        _ => {
            bch_err!(fs, "lost+found in snapshot {snapshot} is not a directory:\n  {}",
                     k.to_text(fs));
            Err(fs.err(bch_errcode::BCH_ERR_ENOENT_not_directory))
        }
    }
}

/// Any subvolume in @tree: master_subvol may have been deleted.
fn find_snapshot_tree_subvol(t: &TransAttempt<'_, '_>, tree: u32) -> Result<u32, BchError> {
    let mut iter = BtreeIter::new(t, c::btree_id::snapshots, POS_MIN, BtreeIterFlags::empty());
    iter.for_each_norestart(t, |_, k| Ok(
        match k.arm::<c::bch_snapshot>().map(|k| k.v()) {
            Some(s) if s.tree.get() == tree && s.subvol.get() != 0 =>
                ControlFlow::Break(Some(s.subvol.get())),
            _ => ControlFlow::Continue(()),
        }))?
        .ok_or_else(|| t.fs().err(bch_errcode::BCH_ERR_ENOENT_no_snapshot_tree_subvol))
}

/// Logs @msg at notice, or at err with the error.
fn create_lostfound_dirent(
    t:          &TransAttempt<'_, '_>,
    msg:        &mut Printbuf,
    root_inum:  c::subvol_inum,
    root_inode: &mut c::bch_inode_unpacked,
    snapshot:   u32,
    inum:       u64,
    dir_offset: &mut u64,
) -> Result<(), BchError> {
    let fs = t.trans().fs();
    let r = dirent::create_snapshot(t, root_inum.subvol as u32, snapshot, root_inode,
                                    c::DT_DIR as u8, OsStr::from_bytes(LOSTFOUND),
                                    DirentTarget::Inode(inum),
                                    dir_offset,
                                    CREATE.0, CREATE.1);
    match &r {
        Err(e) if !is_restart(e) => {
            write!(msg, "\nerror creating dirent: {}", e.msg());
            bch_err!(fs, "{msg}");
        }
        _ => bch_notice!(fs, "{msg}"),
    }
    r
}

/// Create lost+found in @snapshot, the snapshot tree's root.
fn create_lostfound(
    t:          &TransAttempt<'_, '_>,
    snapshot:   u32,
    root_inum:  c::subvol_inum,
    root_inode: &mut c::bch_inode_unpacked,
) -> Result<c::bch_inode_unpacked, BchError> {
    let trans = t.trans();
    let fs = trans.fs();

    let mut msg = Printbuf::new();
    write!(msg, "creating ");
    namei::inum_to_path(trans, root_inum, &mut msg)?;
    write!(msg, "/lost+found in subvol {} snapshot {snapshot}", root_inum.subvol);

    let mut lostfound = inode::init(fs, 0, 0, (c::S_IFDIR | 0o700) as c::umode_t, 0,
                                    Some(root_inode));
    lostfound.bi_dir      = root_inode.bi_inum;
    lostfound.bi_snapshot = snapshot;

    let is_32bit = inode::opts_get_inode(fs, root_inode).inodes_32bit != 0;
    let mut iter = BtreeIter::uninit();
    inode::create(t, &mut iter, &mut lostfound, snapshot, is_32bit)?;

    iter.set_snapshot(snapshot);
    t.iter_traverse(&mut iter)?;

    let inum = lostfound.bi_inum;
    create_lostfound_dirent(t, &mut msg, root_inum, root_inode, snapshot, inum,
                            &mut lostfound.bi_dir_offset)?;

    inode::write_flags(t, &mut iter, &mut lostfound, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
    Ok(lostfound)
}

/// The tree has a lost+found but @snapshot deleted its dirent: give @snapshot a
/// new dirent for the same inode, in whatever slot the hash table picks, rather
/// than removing the whiteout. Different positions in different snapshots are
/// what a directory renamed after a snapshot has anyway.
fn restore_lostfound(
    t:             &TransAttempt<'_, '_>,
    snapshot:      u32,
    root_snapshot: u32,
    root_inum:     c::subvol_inum,
    root_inode:    &mut c::bch_inode_unpacked,
    inum:          u64,
) -> Result<c::bch_inode_unpacked, BchError> {
    let trans = t.trans();
    let fs = trans.fs();

    // Normally still visible here; if the directory was deleted rather than
    // unlinked, only in the root snapshot
    let mut lostfound = bch_err_msg!(fs,
        match inode::find_by_inum_snapshot(trans, inum, snapshot, BtreeIterFlags::empty()) {
            Err(e) if e.matches(c::ENOENT) =>
                inode::find_by_inum_snapshot(trans, inum, root_snapshot, BtreeIterFlags::empty()),
            r => r,
        },
        "looking up lost+found inode {inum} in snapshot {snapshot} or {root_snapshot}")?;

    let mut msg = Printbuf::new();
    write!(msg, "restoring ");
    namei::inum_to_path(trans, root_inum, &mut msg)?;
    write!(msg, "/lost+found in subvol {} snapshot {snapshot}: inode {inum}, deleted here, \
                 still in snapshot {root_snapshot}", root_inum.subvol);

    lostfound.bi_dir      = root_inode.bi_inum;
    lostfound.bi_snapshot = snapshot;

    create_lostfound_dirent(t, &mut msg, root_inum, root_inode, snapshot, inum,
                            &mut lostfound.bi_dir_offset)?;

    inode::fsck_write(t, &mut lostfound)?;
    Ok(lostfound)
}

/// Written at @snapshot, not where the version it reads lives: that would give
/// the link to branches without a lost+found.
fn lostfound_dir_link(t: &TransAttempt<'_, '_>, dir_inum: u64, snapshot: u32)
    -> Result<(), BchError>
{
    let mut dir = inode::find_by_inum_snapshot(t.trans(), dir_inum, snapshot,
                                               BtreeIterFlags::empty())?;
    dir.bi_nlink += 1;
    dir.bi_snapshot = snapshot;
    inode::fsck_write(t, &mut dir)
}

/// @snapshot has no lost+found: the tree has none, or it was deleted here.
fn create_or_restore_lostfound(
    t:          &TransAttempt<'_, '_>,
    tree:       u32,
    snapshot:   u32,
    root_inum:  c::subvol_inum,
    root_inode: &mut c::bch_inode_unpacked,
    root_hash:  &c::bch_hash_info,
) -> Result<c::bch_inode_unpacked, BchError> {
    let trans = t.trans();
    let fs = trans.fs();

    let st = snapshot::tree_lookup(trans, tree)?;
    let tree_root = st.root_snapshot.get();

    let Ok(Some(root_snapshot)) = fs.snapshots.live_descendent(tree_root) else {
        bch_err!(fs, "snapshot tree {tree} has no live snapshot, cannot create lost+found");
        return Err(fs.err(bch_errcode::BCH_ERR_ENOENT_snapshot));
    };

    // One lost+found per tree works only if every branch inherits from where
    // it is
    if !snapshot::is_ancestor(trans, snapshot, root_snapshot) {
        bch_err!(fs, "lost+found for snapshot {snapshot} belongs in snapshot {root_snapshot}, \
                      which it does not inherit from (snapshot tree {tree}, root snapshot {tree_root})");
        return Err(fs.err(bch_errcode::BCH_ERR_snapshot_lostfound_unreachable));
    }

    // Hashing at root_snapshot with @snapshot's root_hash: check_dirents has
    // made every version's hash seed and type the same
    let (lostfound, dirent_snapshot) =
        match lostfound_dirent(t, root_hash, root_inum, root_snapshot)? {
            Some(inum) => (restore_lostfound(t, snapshot, root_snapshot, root_inum, root_inode,
                                             inum)?, snapshot),
            None => (create_lostfound(t, root_snapshot, root_inum, root_inode)?, root_snapshot),
        };

    lostfound_dir_link(t, root_inum.inum, dirent_snapshot)?;
    t.commit_lazy(CommitFlags::NO_ENOSPC)?;
    Ok(lostfound)
}

/// lost+found as @snapshot sees it, created if it doesn't exist.
fn lookup_lostfound(t: &TransAttempt<'_, '_>, snapshot: u32)
    -> Result<c::bch_inode_unpacked, BchError>
{
    let trans = t.trans();
    let fs = trans.fs();
    let tree = fs.snapshots.tree(snapshot);

    let subvolid = bch_err_msg!(fs, find_snapshot_tree_subvol(t, tree),
                                "finding subvol associated with snapshot tree {tree}")?;
    let subvol = bch_err_msg!(fs, subvolume::get_key(trans, subvolid, false),
                              "looking up subvol {subvolid} for snapshot {snapshot}")?;

    let root_inum = c::subvol_inum { subvol: subvolid as u64, inum: subvol.v.inode.get() };

    // The subvolume key's snapshot says whether the root inode is missing or
    // we're looking in the wrong place
    let mut root_inode = bch_err_msg!(fs,
        inode::find_by_inum_snapshot(trans, root_inum.inum, snapshot, BtreeIterFlags::empty()),
        "looking up root inode {} in snapshot {snapshot}, from\n  {}",
        root_inum.inum, BkeySC::from(subvol.k_i()).to_text(fs))?;

    let root_hash = str_hash::hash_info_init(fs, &root_inode)?;

    let Some(inum) = bch_err_msg!(fs, lostfound_dirent(t, &root_hash, root_inum, snapshot),
                                  "looking up lost+found")? else {
        // Commits, so this returns a restart
        return bch_err_msg!(fs, create_or_restore_lostfound(t, tree, snapshot, root_inum,
                                                            &mut root_inode, &root_hash),
                            "getting lost+found for snapshot {snapshot}");
    };

    // check_dirents has already run, dangling dirents shouldn't exist here
    bch_err_msg!(fs,
        inode::find_by_inum_snapshot(trans, inum, snapshot, BtreeIterFlags::empty()),
        "looking up lost+found {inum}:{snapshot} in (root inode {}, snapshot root {})",
        root_inum.inum, fs.snapshots.root(snapshot))
}

/// Whether @inode is unreachable and has to be reattached: no backpointer,
/// not unlinked, and not the root or an old version of a subvolume root.
pub fn inode_should_reattach(inode: &c::bch_inode_unpacked) -> bool {
    if inode.bi_inum == c::BCACHEFS_ROOT_INO as u64 && inode.bi_subvol == c::BCACHEFS_ROOT_SUBVOL {
        return false;
    }

    // A subvolume root has one dirent, in the parent subvolume, unversioned:
    // renamed after a snapshot, its old version is disconnected, as expected.
    // Taking a snapshot always rewrites the root inode, so a version with
    // has_child_snapshot is never the visible one.
    if inode.bi_subvol != 0 && inode.flag(c::bch_inode_flags::BCH_INODE_has_child_snapshot) {
        return false;
    }

    !inode.has_backpointer() && !inode.flag(c::bch_inode_flags::BCH_INODE_unlinked)
}

/// Hide the dirent at @d_pos from @snapshot, a descendant of its snapshot,
/// if it's visible there.
fn maybe_delete_dirent(t: &TransAttempt<'_, '_>, d_pos: c::bpos, snapshot: u32)
    -> Result<(), BchError>
{
    let mut iter = BtreeIter::new(t.trans(), c::btree_id::dirents,
                                  spos(d_pos.inode, d_pos.offset, snapshot),
                                  BtreeIterFlags::INTENT);
    let visible = iter.peek_slot(t)?.expect("a slot always has a key").k.p == d_pos;
    if !visible {
        return Ok(());
    }

    // Explicit: delete_at() needs need_whiteout_for_snapshot() to see the
    // uncommitted dirent, which iterators couldn't when this was written.
    // XXX: they can now - switch once fault injection covers this path.
    let whiteout = t.bkey_alloc_init(0, c::bch_bkey_type::KEY_TYPE_whiteout.0 as u8, iter.pos())?;
    t.update(&iter, &whiteout, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)
}

/// Link @inode into lost+found.
pub fn reattach_inode(t: &TransAttempt<'_, '_>, inode: &mut c::bch_inode_unpacked)
    -> Result<(), BchError>
{
    let trans = t.trans();
    let fs = trans.fs();
    let mut name = Printbuf::new();

    let mut dirent_snapshot = inode.bi_snapshot;
    if inode.bi_subvol != 0 {
        inode.bi_parent_subvol = c::BCACHEFS_ROOT_SUBVOL;

        let mut s = t.bkey_get_mut(c::btree_id::subvolumes, pos(0, inode.bi_subvol as u64),
                                   BtreeIterFlags::empty(), UpdateTriggerFlags::empty(),
                                   c::bch_bkey_type::KEY_TYPE_subvolume,
                                   size_of::<BkeyI<c::bch_subvolume>>())?;
        s.k_i_mut().whole_mut::<c::bch_subvolume>().expect("a subvolume").v.fs_path_parent =
            c::BCACHEFS_ROOT_SUBVOL.into();

        dirent_snapshot = subvolume::get_snapshot(trans, inode.bi_parent_subvol)?;
        write!(name, "subvol-{}", inode.bi_subvol);
    } else {
        write!(name, "{}", inode.bi_inum);
    }

    let mut lostfound = lookup_lostfound(t, dirent_snapshot)?;
    bch_verbose!(fs, "got lostfound inum {}", lostfound.bi_inum);

    // The descendant fixup below commits in chunks, so a re-drive can find our
    // dirent already there: adopt it rather than create a second. A name taken
    // by something else - a file made in lost+found, an earlier repair's entry -
    // is skipped for "<name>.1", "<name>.2", ...: the same sequence every time,
    // so a re-drive finds what it made.
    let lostfound_hash = str_hash::hash_info_init(fs, &lostfound)?;

    let base = name;
    let mut adopted = false;
    let mut chosen = None;
    for i in 0..1000 {
        let mut probe = Printbuf::new();
        probe.write_bytes(base.as_bytes());
        if i > 0 {
            write!(probe, ".{i}");
        }

        let mut d_iter = BtreeIter::uninit();
        let existing = str_hash::lookup_in_snapshot::<Dirents>(
            t, &mut d_iter, &lostfound_hash,
            c::subvol_inum { subvol: inode.bi_parent_subvol as u64, inum: lostfound.bi_inum },
            probe.as_os_str(), BtreeIterFlags::empty(), dirent_snapshot).found()?;

        match existing {
            Some(k) if Dirent::new(k).expect("a dirent").target() == inode.dirent_target() => {
                inode.bi_dir        = lostfound.bi_inum;
                inode.bi_dir_offset = k.k.p.offset;
                adopted = true;
            }
            Some(_) => continue,
            None    => {}
        }
        chosen = Some(probe);
        break;
    }

    let Some(name) = chosen else {
        bch_err!(fs, "reattaching inode {}:{}: lost+found entries {base} through {base}.999 all taken",
                 inode.bi_inum, inode.bi_snapshot);
        return Err(fs.err(bch_errcode::BCH_ERR_fsck_repair_unimplemented));
    };

    // Not is_dir(): a subvolume root's DT_SUBVOL dirent doesn't count towards
    // its parent's links
    if !adopted && inode.is_subdir_for_nlink() {
        lostfound.bi_nlink += 1;
    }

    // lost+found needs a version where the dirent goes, or the dirent is in a
    // snapshot its directory isn't: snapshot_key_missing_inode_snapshot.
    // lookup_lostfound() resolved it from dirent_snapshot: the version found
    // is there or in an ancestor.
    assert!(snapshot::is_ancestor(trans, dirent_snapshot, lostfound.bi_snapshot));
    lostfound.bi_snapshot = dirent_snapshot;

    inode::fsck_write(t, &mut lostfound)?;

    if !adopted {
        inode.bi_dir = lostfound.bi_inum;

        let d_type = inode.d_type();
        let target = inode.dirent_target();
        bch_err_msg!(fs,
            dirent::create_snapshot(t, inode.bi_parent_subvol, dirent_snapshot, &lostfound,
                                    d_type, name.as_os_str(), target, &mut inode.bi_dir_offset,
                                    CREATE.0, CREATE.1),
            "error creating dirent")?;
    }

    inode::fsck_write(t, inode)?;

    let mut path = Printbuf::new();
    namei::inum_snapshot_to_path(trans, inode.bi_inum, inode.bi_snapshot, &mut path)?;
    if adopted {
        bch_verbose!(fs, "resuming reattach at {path}");
    } else {
        bch_info!(fs, "reattached at {path}");
    }

    // Descendant versions: point those that also need reattaching at the new
    // dirent, and hide it from the rest
    if inode.bi_subvol != 0 || fs.snapshots.is_leaf(inode.bi_snapshot)? {
        return Ok(());
    }

    let mut whiteouts_done: KVVec<u32> = KVVec::new();
    let mut iter = BtreeIter::new(trans, c::btree_id::inodes,
                                  spos(0, inode.bi_inum, inode.bi_snapshot - 1),
                                  BtreeIterFlags::ALL_SNAPSHOTS | BtreeIterFlags::INTENT);
    while let Some(k) = iter.peek_prev_min(t, pos(0, inode.bi_inum))? {
        let id = k.k.p.snapshot;
        let child = inode::bkey_is_inode(k.k).then(|| inode::unpack(fs, k));

        // An update per version can overflow the transaction: commit as we go.
        // The restart re-drives us, the adopt path above resumes, and versions
        // already fixed are skipped below.
        t.commit_lazy_if_full(CommitFlags::NO_ENOSPC)?;

        let child = child.filter(|child| {
            snapshot::is_ancestor(trans, id, inode.bi_snapshot) &&
            !whiteouts_done.iter().any(|&w| snapshot::is_ancestor(trans, id, w)) &&
            // Fixed by an earlier partial commit. Before inode_should_reattach():
            // with a backpointer, it would take the whiteout arm and dangle
            (child.bi_dir, child.bi_dir_offset) != (inode.bi_dir, inode.bi_dir_offset)
        });

        if let Some(mut child) = child {
            if inode_should_reattach(&child) {
                iter.set_snapshot(id);
                child.bi_dir        = inode.bi_dir;
                child.bi_dir_offset = inode.bi_dir_offset;
                inode::write_flags(t, &mut iter, &mut child,
                                   UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
            } else {
                maybe_delete_dirent(t, spos(lostfound.bi_inum, inode.bi_dir_offset,
                                            dirent_snapshot), id)?;
                whiteouts_done.push(id, GFP_KERNEL)?;
            }
        }

        if !iter.rewind() {
            break;
        }
    }

    Ok(())
}
