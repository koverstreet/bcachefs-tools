// SPDX-License-Identifier: GPL-2.0

//! fsck: lost+found, and reattaching unreachable inodes in it.
//!
//! There's one lost+found per snapshot tree, in the tree's root snapshot, so
//! every branch inherits the same one; lookup_lostfound() finds it from a
//! snapshot, recreating it if the tree has none, or restoring its dirent in a
//! snapshot that deleted it. reattach_inode() names an inode there - by its
//! number, or "subvol-<n>" for a subvolume root - and then fixes up the
//! versions of the inode in descendant snapshots: pointing those that also
//! need reattaching at the new dirent, and hiding it from those that don't.
//!
//! The descendant fixup commits as it goes when there's a lot of it, so a
//! retry can find the reattach dirent already committed: it adopts it rather
//! than creating a second.
//!
//! Changes from the C:
//!
//! - lookup_lostfound() had a branch for a subvolume key with no root inode,
//!   which set the subvolume's root to the inode being reattached - then
//!   looked up root inode 0, through a local copy of the key it had shadowed.
//!   bch2_subvolume_validate() rejects such a key (subvol_inode_bad), so the
//!   branch couldn't run; it's gone.
//!
//! - Whether the inode's snapshot is a leaf, for the descendant fixup: an
//!   error there - no such snapshot - was taken as "not a leaf"; it's
//!   returned now. check_key_has_snapshot() should have dealt with a key in a
//!   missing snapshot before we got here.
//!
//! - Adopting an existing reattach dirent: the C flattened what it points at
//!   to one number - subvolume ID or inode number by d_type - and compared
//!   it with the inode's, so inode 5 would adopt a DT_SUBVOL dirent for
//!   subvolume 5. It compares DirentTargets now, and that's "points
//!   elsewhere".
//!
//! - A lost+found entry under the inode's name that points elsewhere was
//!   fsck_repair_unimplemented, stopping fsck; the name is probed forward
//!   ("<n>.1", ...) to the first that's ours or free.
//!
//! This code has been troublesome; fsck-inject has a test for each path:
//! creating vs restoring lost+found, an adopted reattach dirent (the
//! partial-commit re-drive), a taken name, the descendant fixup with its
//! whiteouts.

use crate::btree::bkey::{pos, spos, BkeySC, POS_MIN};
use crate::btree::iter::{
    BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, TransAttempt, TransError, TransResult,
    TransRet, UpdateTriggerFlags,
};
use crate::c;
use crate::dirent::{self, DirentTarget, Dirents};
use crate::errcode::{bch_errcode, BchError, Found};
use crate::inode;
use crate::namei;
use crate::snapshots::{snapshot, subvolume};
use crate::str_hash;
use crate::util::alloc::{flags::GFP_KERNEL, KVVec};
use crate::util::Printbuf;
use crate::{bch_err, bch_err_msg, bch_info, bch_notice, bch_verbose};
use core::mem::size_of;
use core::ops::ControlFlow;

const LOSTFOUND: &[u8] = b"lost+found";

/// The flags lost+found's dirents are created with.
const CREATE: (BtreeIterFlags, UpdateTriggerFlags) =
    (BtreeIterFlags::STR_HASH_MUST_CREATE, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE);

/// The inode lost+found's dirent in @root - hashed with @root_hash - points
/// at, as seen from @snapshot; None if there's no such dirent.
/// ENOENT_not_directory if it isn't a directory - an error, not a None: we'd
/// fail to create one over it.
fn lostfound_dirent(
    trans:     &BtreeTrans<'_>,
    root_hash: &c::bch_hash_info,
    root:      c::subvol_inum,
    snapshot:  u32,
) -> Result<Option<u64>, BchError> {
    let fs = trans.fs();
    let mut iter = BtreeIter::uninit();
    let Some(k) = str_hash::lookup_in_snapshot::<Dirents>(trans, &mut iter, root_hash, root,
                                                           &dirent::qstr(LOSTFOUND),
                                                           BtreeIterFlags::empty(), snapshot)
        .found()? else { return Ok(None) };

    let d = k.as_dirent().expect("a dirent");
    if d.d_type() as u32 != c::DT_DIR {
        bch_err!(fs, "lost+found in snapshot {snapshot} is not a directory:\n  {}", k.to_text(fs));
        return Err(fs.err(bch_errcode::BCH_ERR_ENOENT_not_directory));
    }
    Ok(Some(d.d_inum()))
}

/// Any subvolume in snapshot tree @tree - not master_subvol, which might
/// have been deleted.
fn find_snapshot_tree_subvol(trans: &BtreeTrans<'_>, tree: u32) -> Result<u32, BchError> {
    let mut subvol = 0;
    let mut iter = BtreeIter::new(trans, c::btree_id::snapshots, POS_MIN, BtreeIterFlags::empty());
    iter.for_each_norestart(|_, k| {
        if let Some(s) = k.as_snapshot() {
            if u32::from_le(s.tree) == tree && s.subvol != 0 {
                subvol = u32::from_le(s.subvol);
                return Ok(ControlFlow::Break(()));
            }
        }
        Ok(ControlFlow::Continue(()))
    })?;

    if subvol == 0 {
        return Err(trans.fs().err(bch_errcode::BCH_ERR_ENOENT_no_snapshot_tree_subvol));
    }
    Ok(subvol)
}

/// Name directory @inum lost+found in @root at @snapshot, logging @msg - what
/// we're doing - at notice, or at err with the error if it fails.
fn create_lostfound_dirent<'a, 't>(
    t:          TransAttempt<'a, 't>,
    msg:        &mut Printbuf,
    root_inum:  c::subvol_inum,
    root_inode: &mut c::bch_inode_unpacked,
    snapshot:   u32,
    inum:       u64,
    dir_offset: &mut u64,
) -> TransRet<'a, 't> {
    let fs = t.trans().fs();
    let r = dirent::create_snapshot(t, root_inum.subvol as u32, snapshot, root_inode,
                                    c::DT_DIR as u8, LOSTFOUND, DirentTarget::Inode(inum),
                                    dir_offset,
                                    CREATE.0, CREATE.1);
    match &r {
        Err(TransError::Error(e)) => {
            write!(msg, "\nerror creating dirent: {}", e.msg());
            bch_err!(fs, "{msg}");
        }
        _ => bch_notice!(fs, "{msg}"),
    }
    r
}

/// Create lost+found in @snapshot, the snapshot tree's root.
fn create_lostfound<'a, 't>(
    t:          TransAttempt<'a, 't>,
    snapshot:   u32,
    root_inum:  c::subvol_inum,
    root_inode: &mut c::bch_inode_unpacked,
) -> TransResult<'a, 't, c::bch_inode_unpacked> {
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
    let t = inode::create(t, &mut iter, &mut lostfound, snapshot, is_32bit)?;

    iter.set_snapshot(snapshot);
    let t = t.iter_traverse(&mut iter)?;

    let inum = lostfound.bi_inum;
    let t = create_lostfound_dirent(t, &mut msg, root_inum, root_inode, snapshot, inum,
                                    &mut lostfound.bi_dir_offset)?;

    let t = inode::write_flags(t, &mut iter, &mut lostfound,
                               UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
    t.done(lostfound)
}

/// The snapshot tree has a lost+found, but @snapshot hasn't: it was deleted
/// there. Give @snapshot a dirent for the same inode - one lost+found per
/// tree is the invariant, not one dirent.
///
/// A fresh dirent, deliberately, rather than removing the whiteout in place:
/// dirents are a hash table, so let the table pick the slot. It goes back
/// into the slot that was freed in the normal case, and probes past whatever
/// took it otherwise. One inode with different dirent positions in different
/// snapshots is what a directory renamed after a snapshot already looks like.
fn restore_lostfound<'a, 't>(
    t:             TransAttempt<'a, 't>,
    snapshot:      u32,
    root_snapshot: u32,
    root_inum:     c::subvol_inum,
    root_inode:    &mut c::bch_inode_unpacked,
    inum:          u64,
) -> TransResult<'a, 't, c::bch_inode_unpacked> {
    let trans = t.trans();
    let fs = trans.fs();

    // The inode is normally still visible here - only the dirent was
    // shadowed - but if the directory was deleted rather than unlinked it's
    // shadowed too, and we have to go get it from the root snapshot.
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

    let t = create_lostfound_dirent(t, &mut msg, root_inum, root_inode, snapshot, inum,
                                    &mut lostfound.bi_dir_offset)?;

    let t = inode::fsck_write(t, &mut lostfound)?;
    t.done(lostfound)
}

/// lost+found is a subdirectory of the root inode in @snapshot, so the root
/// inode gains a link there. Take it on the version @snapshot sees and write
/// it back at @snapshot: writing it where that version lives would hand the
/// link to sibling branches that haven't got a lost+found.
fn lostfound_dir_link<'a, 't>(t: TransAttempt<'a, 't>, dir_inum: u64, snapshot: u32)
    -> TransRet<'a, 't>
{
    let mut dir = inode::find_by_inum_snapshot(t.trans(), dir_inum, snapshot,
                                               BtreeIterFlags::empty())?;
    dir.bi_nlink += 1;
    dir.bi_snapshot = snapshot;
    inode::fsck_write(t, &mut dir)
}

/// @snapshot needs a lost+found and hasn't got one. There's one per snapshot
/// tree, in the tree's root snapshot so that every branch inherits the same
/// one, so either the tree hasn't got one at all or it has and it was deleted
/// here.
fn create_or_restore_lostfound<'a, 't>(
    t:          TransAttempt<'a, 't>,
    tree:       u32,
    snapshot:   u32,
    root_inum:  c::subvol_inum,
    root_inode: &mut c::bch_inode_unpacked,
    root_hash:  &c::bch_hash_info,
) -> TransResult<'a, 't, c::bch_inode_unpacked> {
    let trans = t.trans();
    let fs = trans.fs();

    let st = snapshot::tree_lookup(trans, tree)?;
    let tree_root = u32::from_le(st.root_snapshot);

    let Ok(Some(root_snapshot)) = snapshot::live_descendent(fs, tree_root) else {
        bch_err!(fs, "snapshot tree {tree} has no live snapshot, cannot create lost+found");
        return Err(fs.err(bch_errcode::BCH_ERR_ENOENT_snapshot).into());
    };

    // Keeping lost+found in the root snapshot only gives every branch the
    // same one if they all inherit from it. If this snapshot doesn't, we can
    // neither find what's there nor create something it will see.
    if !snapshot::is_ancestor(trans, snapshot, root_snapshot) {
        bch_err!(fs, "lost+found for snapshot {snapshot} belongs in snapshot {root_snapshot}, \
                      which it does not inherit from (snapshot tree {tree}, root snapshot {tree_root})");
        return Err(fs.err(bch_errcode::BCH_ERR_snapshot_lostfound_unreachable).into());
    }

    // root_hash came from the root inode as @snapshot sees it, and we're about
    // to hash with it in another snapshot: fine, because all versions of an
    // inode must have the same hash seed and type, and check_dirents has
    // already run and repaired any that didn't.
    let (t, lostfound, dirent_snapshot) =
        match lostfound_dirent(trans, root_hash, root_inum, root_snapshot)? {
            Some(inum) => {
                let (t, l) = restore_lostfound(t, snapshot, root_snapshot, root_inum, root_inode,
                                               inum)?;
                (t, l, snapshot)
            }
            None => {
                let (t, l) = create_lostfound(t, root_snapshot, root_inum, root_inode)?;
                (t, l, root_snapshot)
            }
        };

    let t = lostfound_dir_link(t, root_inum.inum, dirent_snapshot)?;
    let t = t.commit_lazy(CommitFlags::NO_ENOSPC)?;
    t.done(lostfound)
}

/// lost+found as @snapshot sees it, created if it doesn't exist.
fn lookup_lostfound<'a, 't>(t: TransAttempt<'a, 't>, snapshot: u32)
    -> TransResult<'a, 't, c::bch_inode_unpacked>
{
    let trans = t.trans();
    let fs = trans.fs();
    let tree = snapshot::tree(fs, snapshot);

    let subvolid = bch_err_msg!(fs, find_snapshot_tree_subvol(trans, tree),
                                "finding subvol associated with snapshot tree {tree}")?;
    let subvol = bch_err_msg!(fs, subvolume::get_key(trans, subvolid, false),
                              "looking up subvol {subvolid} for snapshot {snapshot}")?;

    let root_inum = c::subvol_inum { subvol: subvolid as u64, inum: u64::from_le(subvol.v.inode) };

    // The inum came out of the subvolume key, so print the key: which snapshot
    // it points at is what says whether the root inode is missing or we're
    // looking in the wrong place.
    let mut root_inode = bch_err_msg!(fs,
        inode::find_by_inum_snapshot(trans, root_inum.inum, snapshot, BtreeIterFlags::empty()),
        "looking up root inode {} in snapshot {snapshot}, from\n  {}",
        root_inum.inum, BkeySC::from(subvol.k_i()).to_text(fs))?;

    let root_hash = str_hash::hash_info_init(fs, &root_inode)?;

    let Some(inum) = bch_err_msg!(fs, lostfound_dirent(trans, &root_hash, root_inum, snapshot),
                                  "looking up lost+found")? else {
        // We always create lost+found in its own transaction; this will
        // return a transaction restart:
        return bch_err_msg!(fs, create_or_restore_lostfound(t, tree, snapshot, root_inum,
                                                            &mut root_inode, &root_hash),
                            "getting lost+found for snapshot {snapshot}");
    };

    // check_dirents has already run, dangling dirents shouldn't exist here
    let lostfound = bch_err_msg!(fs,
        inode::find_by_inum_snapshot(trans, inum, snapshot, BtreeIterFlags::empty()),
        "looking up lost+found {inum}:{snapshot} in (root inode {}, snapshot root {})",
        root_inum.inum, snapshot::root(fs, snapshot))?;
    t.done(lostfound)
}

/// Whether @inode is unreachable and has to be reattached: no backpointer,
/// not unlinked, and not the root or an old version of a subvolume root.
pub fn inode_should_reattach(inode: &c::bch_inode_unpacked) -> bool {
    if inode.bi_inum == c::BCACHEFS_ROOT_INO as u64 && inode.bi_subvol == c::BCACHEFS_ROOT_SUBVOL {
        return false;
    }

    // Subvolume roots are special: older versions of subvolume roots may be
    // disconnected, it's only the newest version that matters.
    //
    // We only keep a single dirent pointing to a subvolume root, i.e. older
    // versions of snapshots will not have a different dirent pointing to the
    // same subvolume root.
    //
    // This is because dirents that point to subvolumes are only visible in
    // the parent subvolume - versioning is not needed - and keeping them
    // around would break fsck, because when we're crossing subvolumes we
    // don't have a consistent snapshot ID to do check the inode <-> dirent
    // relationships.
    //
    // Thus, a subvolume root that's been renamed after a snapshot will have a
    // disconnected older version - that's expected.
    //
    // Note that taking a snapshot always updates the root inode (to update
    // the dirent backpointer), so a subvolume root inode with
    // BCH_INODE_has_child_snapshot is never visible.
    if inode.bi_subvol != 0 && inode.flag(c::bch_inode_flags::BCH_INODE_has_child_snapshot) {
        return false;
    }

    !inode::has_backpointer(inode) && !inode.flag(c::bch_inode_flags::BCH_INODE_unlinked)
}

/// Hide the dirent at @d_pos from @snapshot, a descendant of its snapshot,
/// if it's visible there.
fn maybe_delete_dirent<'a, 't>(t: TransAttempt<'a, 't>, d_pos: c::bpos, snapshot: u32)
    -> TransRet<'a, 't>
{
    let mut iter = BtreeIter::new(t.trans(), c::btree_id::dirents,
                                  spos(d_pos.inode, d_pos.offset, snapshot),
                                  BtreeIterFlags::INTENT);
    let visible = iter.peek_slot()?.expect("a slot always has a key").k.p == d_pos;
    if !visible {
        return Ok(t);
    }

    // An explicit whiteout because delete_at() relies on
    // need_whiteout_for_snapshot() to see the dirent we just created, which
    // wasn't possible when this was written: it's still an uncommitted
    // update, and iterators didn't see those then.
    //
    // XXX: they do now, so delete_at() should be equivalent - switch once a
    // fault injection test covers this path.
    let whiteout = t.bkey_alloc_init(0, c::bch_bkey_type::KEY_TYPE_whiteout.0 as u8, iter.pos())?;
    t.update(&mut iter, whiteout, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)
}

/// Link @inode into lost+found.
pub fn reattach_inode<'a, 't>(t: TransAttempt<'a, 't>, inode: &mut c::bch_inode_unpacked)
    -> TransRet<'a, 't>
{
    let trans = t.trans();
    let fs = trans.fs();
    let mut name = Printbuf::new();

    let mut dirent_snapshot = inode.bi_snapshot;
    if inode.bi_subvol != 0 {
        inode.bi_parent_subvol = c::BCACHEFS_ROOT_SUBVOL;

        let mut s = t.bkey_get_mut(c::btree_id::subvolumes, pos(0, inode.bi_subvol as u64),
                                   UpdateTriggerFlags::empty(),
                                   c::bch_bkey_type::KEY_TYPE_subvolume,
                                   size_of::<c::bkey_i_subvolume>())?;
        s.k_i_mut().as_mut_subvolume().expect("a subvolume").fs_path_parent =
            c::BCACHEFS_ROOT_SUBVOL.to_le();

        dirent_snapshot = subvolume::get_snapshot(trans, inode.bi_parent_subvol)?;
        write!(name, "subvol-{}", inode.bi_subvol);
    } else {
        write!(name, "{}", inode.bi_inum);
    }

    let (t, mut lostfound) = lookup_lostfound(t, dirent_snapshot)?;
    bch_verbose!(fs, "got lostfound inum {}", lostfound.bi_inum);

    // Adopt instead of create: the child fixup loop below commits in chunks
    // (commit_lazy_if_full()), so a re-drive can find the reattach dirent
    // already committed - at our snapshot or an ancestor, when the committed
    // fixups moved the oldest-needing-reattach point down. The name is
    // deterministic, so look it up: adopting avoids the STR_HASH_MUST_CREATE
    // collision and re-bumping lost+found's nlink.
    let lostfound_hash = str_hash::hash_info_init(fs, &lostfound)?;

    //
    // A name that's taken by something else - a file someone made in
    // lost+found, or an entry an earlier repair left - isn't ours: try
    // "<name>.1", "<name>.2", ..., adopting the first that points at us, or
    // taking the first free one. Deterministic, so a re-drive probes the same
    // names in the same order and finds what it made.
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
            trans, &mut d_iter, &lostfound_hash,
            c::subvol_inum { subvol: inode.bi_parent_subvol as u64, inum: lostfound.bi_inum },
            &dirent::qstr(probe.as_bytes()), BtreeIterFlags::empty(), dirent_snapshot).found()?;

        match existing {
            Some(k) if k.as_dirent().expect("a dirent").target() == inode.dirent_target() => {
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
        return Err(fs.err(bch_errcode::BCH_ERR_fsck_repair_unimplemented).into());
    };

    // is_subdir_for_nlink(), not is_dir(): a subvolume root is named by a
    // DT_SUBVOL dirent, which doesn't count towards its parent's link count.
    // Bumping it here for one leaves check_nlinks() to disagree.
    if !adopted && inode.is_subdir_for_nlink() {
        lostfound.bi_nlink += 1;
    }

    // Ensure lost+found has an inode version in the snapshot we're about to
    // create the dirent in, or we leave a key in a snapshot whose inode only
    // exists in an ancestor - snapshot_key_missing_inode_snapshot, which the
    // next check_dirents has to clean up after us.
    //
    // dirent_snapshot is the inode's own snapshot for an ordinary inode, and
    // the parent subvolume's for a subvolume root (above); lookup_lostfound()
    // resolved lost+found from it, so it is at worst an ancestor of it.
    assert!(snapshot::is_ancestor(trans, dirent_snapshot, lostfound.bi_snapshot));
    lostfound.bi_snapshot = dirent_snapshot;

    let mut t = inode::fsck_write(t, &mut lostfound)?;

    if !adopted {
        inode.bi_dir = lostfound.bi_inum;

        let d_type = inode.d_type();
        let target = inode.dirent_target();
        t = bch_err_msg!(fs,
            dirent::create_snapshot(t, inode.bi_parent_subvol, dirent_snapshot, &mut lostfound,
                                    d_type, name.as_bytes(), target, &mut inode.bi_dir_offset,
                                    CREATE.0, CREATE.1),
            "error creating dirent")?;
    }

    t = inode::fsck_write(t, inode)?;

    let mut path = Printbuf::new();
    namei::inum_snapshot_to_path(trans, inode.bi_inum, inode.bi_snapshot, &mut path)?;
    if adopted {
        bch_verbose!(fs, "resuming reattach at {path}");
    } else {
        bch_info!(fs, "reattached at {path}");
    }

    // Fix up inodes in child snapshots: if they should also be reattached
    // update the backpointer field, if they should not be we need to emit
    // whiteouts for the dirent we just created.
    if inode.bi_subvol != 0 || snapshot::is_leaf(fs, inode.bi_snapshot)? {
        return Ok(t);
    }

    let mut whiteouts_done: KVVec<u32> = KVVec::new();
    let mut iter = BtreeIter::new(trans, c::btree_id::inodes,
                                  spos(0, inode.bi_inum, inode.bi_snapshot - 1),
                                  BtreeIterFlags::ALL_SNAPSHOTS | BtreeIterFlags::INTENT);
    while let Some(k) = iter.peek_prev_min(pos(0, inode.bi_inum))? {
        let id = k.k.p.snapshot;
        let child = inode::bkey_is_inode(k.k).then(|| inode::unpack(fs, k));

        // This loop batches an update per descendant snapshot version into
        // one transaction; a fat chain overflows the bump allocator. Commit
        // once substantial work has accumulated - the restart re-drives us,
        // the adopt path above resumes without duplicating the reattach
        // dirent, and already-fixed children are skipped below:
        t = t.commit_lazy_if_full(CommitFlags::NO_ENOSPC)?;

        let child = child.filter(|child| {
            snapshot::is_ancestor(trans, id, inode.bi_snapshot) &&
            !whiteouts_done.iter().any(|&w| snapshot::is_ancestor(trans, id, w)) &&
            // Fixed by a previous partial commit: its backpointer already
            // names our reattach dirent. Must be checked before
            // inode_should_reattach() - having a backpointer, it would fall
            // into the whiteout arm and turn the committed fixup into a
            // dangling backpointer:
            (child.bi_dir, child.bi_dir_offset) != (inode.bi_dir, inode.bi_dir_offset)
        });

        if let Some(mut child) = child {
            if inode_should_reattach(&child) {
                iter.set_snapshot(id);
                child.bi_dir        = inode.bi_dir;
                child.bi_dir_offset = inode.bi_dir_offset;
                t = inode::write_flags(t, &mut iter, &mut child,
                                       UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
            } else {
                t = maybe_delete_dirent(t, spos(lostfound.bi_inum, inode.bi_dir_offset,
                                                dirent_snapshot), id)?;
                whiteouts_done.push(id, GFP_KERNEL)?;
            }
        }

        if !iter.rewind() {
            break;
        }
    }

    Ok(t)
}
