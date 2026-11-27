// SPDX-License-Identifier: GPL-2.0

//! Helpers shared by the fsck passes.

use crate::btree::bkey::{pos, spos, BkeyI, BkeySC};
use crate::btree::bkey::prelude::*;
use crate::btree::iter::{
    BtreeIter, BtreeIterFlags, CommitFlags, TransAttempt, UpdateTriggerFlags,
};
use crate::c;
use crate::posix::{inode::InodeUnpacked, inode, dirent::{Dirent, DirentTarget}};
use crate::posix::dev::dev_t;
use crate::posix::mode::{umode_t, S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT, S_IFREG, S_IFSOCK};
use crate::snapshots::format::{Snapshot, SnapshotState, SnapshotTree, Subvolume, SubvolumeState};
use crate::errcode::{Errcode, BchError, Found};
use crate::fs::Fs;
use crate::snapshots::{snapshot, subvolume};
use crate::alloc::capacity::DiskReservation;
use crate::util::{alloc::Vec, Printbuf};
use crate::{message, fsck_err_on, inode_fsck_err, prt};
use core::ops::ControlFlow;
use crate::format::{Bpos, Btree, Whiteout};

/// The snapshots of the keys seen so far at one position, while walking a
/// btree in key order with all snapshots: a key in an ancestor is hidden from
/// a descendant that overwrote it. C only passes it through, from check_key()
/// back to str_hash's check_key().
pub struct SnapshotsSeen {
    pos: Bpos,
    /// Ascending, as seen in key order, plus any a repair added in order.
    ids: Vec<u32>,
}

impl SnapshotsSeen {
    pub fn new() -> Self {
        SnapshotsSeen { pos: Default::default(), ids: Vec::new() }
    }

    /// Record a key at @pos; a new inode:offset starts the list over.
    pub fn update(&mut self, pos: Bpos) -> Result<(), BchError> {
        if self.pos.inode != pos.inode || self.pos.offset != pos.offset {
            self.ids.clear();
        }
        self.pos = pos;

        if !self.has_id(pos.snapshot) {
            self.ids.push(pos.snapshot)?;
        }
        Ok(())
    }

    /// Whether a key in @ancestor is visible in its descendant @id: not
    /// overwritten by a key this list has seen.
    pub fn key_visible(&self, t: &TransAttempt<'_, '_>, id: u32, ancestor: u32) -> bool {
        debug_assert!(id <= ancestor);

        if id == ancestor {
            return true;
        }
        if !snapshot::is_ancestor(t, id, ancestor) {
            return false;
        }

        // Overwritten by a key seen in a snapshot between the two?
        !self.ids.iter().any(|&i| i != ancestor && snapshot::is_ancestor(t, id, i))
    }

    /// Whether a key in @src points at something visible as a key in @dst, in
    /// some snapshot. The @src keys must be walked in key order, recorded here.
    pub fn ref_visible(&self, t: &TransAttempt<'_, '_>, src: u32, dst: u32) -> bool {
        if dst <= src {
            self.key_visible(t, dst, src)
        } else {
            snapshot::is_ancestor(t, src, dst)
        }
    }

    /// The snapshots that have overwritten the key at @pos in @btree, as if a walk
    /// had seen them: for a key checked out of walk order.
    pub fn overwrites(t: &TransAttempt<'_, '_>, btree: Btree, pos: Bpos)
        -> Result<Self, BchError>
    {
        Ok(SnapshotsSeen { pos, ids: snapshot::overwrites(t, btree, pos)? })
    }

    /// Whether a key in snapshot @id has been seen at this position.
    pub fn has_id(&self, id: u32) -> bool {
        self.ids.contains(&id)
    }

    /// A copy, to keep the list as it was at this key while the walk goes on.
    pub fn try_clone(&self) -> Result<Self, BchError> {
        let mut ids = Vec::with_capacity(self.ids.len())?;
        for &i in self.ids.iter() {
            ids.push(i)?;
        }
        Ok(SnapshotsSeen { pos: self.pos, ids })
    }

    /// For a key a repair created at this position: kept sorted.
    pub fn add_inorder(&mut self, id: u32) -> Result<(), BchError> {
        match self.ids.iter().position(|&i| i >= id) {
            Some(idx) if self.ids[idx] == id => {}
            Some(idx) => self.ids.insert(idx, id)?,
            None => self.ids.push(id)?,
        }
        Ok(())
    }
}

/// Whether a key in @src and one in @dst are both visible in some snapshot:
/// one an ancestor of the other, not overwritten in between.
pub fn ref_visible2(
    t:        &TransAttempt<'_, '_>,
    src:      u32,
    src_seen: &SnapshotsSeen,
    dst:      u32,
    dst_seen: &SnapshotsSeen,
) -> bool {
    if dst > src {
        dst_seen.key_visible(t, src, dst)
    } else {
        src_seen.key_visible(t, dst, src)
    }
}

/// Every version of the inode the walked keys belong to, for passes walking a
/// btree keyed by inode number (xattrs, dirents, extents) with all snapshots.
/// Cached between keys of one inode, refetched on the next inode or after a
/// commit; sorted by snapshot ID, each with the pass's counts (i_sectors,
/// subdirectories).
pub struct InodeWalker {
    first_this_inode: bool,
    have_inodes:      bool,
    recalculate_sums: bool,
    last_pos:         Bpos,
    /// The versions are valid while the transaction's commit_count is this.
    commit_count:     u32,
    inodes:           Vec<WalkerEntry>,
    /// From get_visible(): snapshots where a whiteout hides older versions.
    deletes:          Vec<u32>,
    /// A repair changed an inode's type: see take_changed_inode_type().
    changed_inode_type: bool,
}

/// One version of the walked inode - or a whiteout, where the inode was
/// deleted in that snapshot - and what the pass has counted against it.
#[derive(Clone, Copy)]
pub struct WalkerEntry {
    pub inode:    InodeUnpacked,
    pub whiteout: bool,
    pub count:    u64,
}

impl WalkerEntry {
    fn new(fs: &Fs, k: BkeySC<'_>) -> Self {
        let (inode, whiteout) = match k.arm::<inode::Inodes>() {
            Some(i) => (inode::unpack(fs, i), false),
            None    => (InodeUnpacked {
                bi_inum:     k.k.p.offset,
                bi_snapshot: k.k.p.snapshot,
                ..Default::default()
            }, true),
        };
        WalkerEntry { inode, whiteout, count: 0 }
    }
}

/// The live inode version a key belongs to, from InodeWalker::walk().
pub struct Walked<'w> {
    pub inode:            &'w mut InodeUnpacked,
    /// The first key walked in this inode: per-inode state such as the
    /// bch_hash_info is set up here. Reading it clears it.
    pub first_this_inode: bool,
}

impl InodeWalker {
    pub fn new() -> Self {
        InodeWalker {
            first_this_inode: false,
            have_inodes:      false,
            recalculate_sums: false,
            last_pos:         Default::default(),
            commit_count:     0,
            inodes:           Vec::new(),
            deletes:          Vec::new(),
            changed_inode_type: false,
        }
    }

    /// The live version @k belongs to: None if @k is a tombstone, or its inode is
    /// missing or a whiteout. A missing inode, or one of the wrong type, is
    /// reported and repaired, which may commit and return a restart; on an error
    /// the cache is invalidated, so the retry reads the btree.
    pub fn walk<'w, 't>(
        &'w mut self,
        t:    &TransAttempt<'_, 't>,
        iter: &BtreeIter<'t>,
        k:    BkeySC<'_>,
    ) -> Result<Option<Walked<'w>>, BchError> {
        let i = match self.walk_and_check(t, iter, k) {
            Ok(i) => i,
            Err(e) => {
                // Keep last_pos on this inode: a retry that saw a new inode
                // would have the caller check this one's sums half counted
                self.last_pos = k.k.p;
                self.invalidate();
                return Err(e);
            }
        };

        let Some(i) = i.filter(|&i| !self.inodes[i].whiteout) else { return Ok(None) };

        let first_this_inode = core::mem::take(&mut self.first_this_inode);
        Ok(Some(Walked { inode: &mut self.inodes[i].inode, first_this_inode }))
    }

    /// The index of the version @k resolves to, if any.
    fn walk_and_check<'t>(
        &mut self,
        t:    &TransAttempt<'_, 't>,
        iter: &BtreeIter<'t>,
        k:    BkeySC<'_>,
    ) -> Result<Option<usize>, BchError> {
        if self.last_pos.inode != k.k.p.inode {
            self.get_inodes_all_snapshots(t, k.k.p.inode)?;
        } else if self.commit_count != t.commit_count() {
            // A commit may have changed cached inodes: refetch, and since we're partway
            // through this inode's keys, recount rather than complain
            self.get_inodes_all_snapshots(t, k.k.p.inode)?;
            self.recalculate_sums = true;
        }

        self.last_pos = k.k.p;

        let i = self.lookup_inode_for_snapshot(t, k)?;
        self.check_key_has_inode(t, iter, i, k)?;
        Ok(i)
    }

    /// Load every version of inode @inum, whiteouts included, in snapshot ID
    /// order.
    fn get_inodes_all_snapshots(&mut self, t: &TransAttempt<'_, '_>, inum: u64)
        -> Result<(), BchError>
    {
        let fs = t.fs();

        // No inodes for last_pos now: clear it, or a restart here would confuse
        // check_i_sectors and check_subdir_count
        self.have_inodes = false;
        self.recalculate_sums = false;
        self.inodes.clear();

        let inodes = &mut self.inodes;
        let mut iter = BtreeIter::new(t, Btree::inodes, pos(0, inum),
                                      BtreeIterFlags::ALL_SNAPSHOTS);
        iter.for_each_max_norestart(t, spos(0, inum, u32::MAX), |_, k| {
            inodes.push(WalkerEntry::new(fs, k))?;
            Ok(())
        })?;

        self.first_this_inode = true;
        self.have_inodes = true;
        self.commit_count = t.commit_count();
        Ok(())
    }

    /// The version @k belongs to: the nearest, in its snapshot or an ancestor. It
    /// should be @k's own, a key's inode being updated with it; if it's an
    /// ancestor's, the inode (or whiteout) is copied into @k's snapshot,
    /// committed, and a restart returned.
    fn lookup_inode_for_snapshot(&mut self, t: &TransAttempt<'_, '_>, k: BkeySC<'_>)
        -> Result<Option<usize>, BchError>
    {
        let fs = t.fs();
        let snapshot = k.k.p.snapshot;
        let k_snapshot = fs.snapshots.redundant_interior(snapshot).unwrap_or(snapshot);

        let Some(i) = self.inodes.iter()
            .position(|i| snapshot::is_ancestor(t, k_snapshot, i.inode.bi_snapshot)) else {
            return Ok(None);
        };

        let e = self.inodes[i];
        let inode_snapshot = fs.snapshots.redundant_interior(e.inode.bi_snapshot)
            .unwrap_or(e.inode.bi_snapshot);

        if !fsck_err_on!(t, k_snapshot != inode_snapshot, snapshot_key_missing_inode_snapshot,
                         "have key for inode {}:{snapshot} but have inode in ancestor snapshot {}\n\
                          unexpected because we should always update the inode when we update a key in that inode\n\
                          {}",
                         { self.last_pos.inode }, e.inode.bi_snapshot, k.to_text(fs))? {
            return Ok(Some(i));
        }

        if !e.whiteout {
            let mut new = e.inode;
            new.bi_snapshot = snapshot;
            inode::fsck_write(t, &mut new)?;
        } else {
            let whiteout = t.bkey_alloc_init::<Whiteout>(0,
                                             spos(0, e.inode.bi_inum, snapshot))?;
            t.insert_with(Btree::inodes, whiteout, BtreeIterFlags::CACHED,
                          UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
        }
        t.commit(None, CommitFlags::empty())?;

        // walk() invalidates on the restart: the retry refetches the versions,
        // the one just written included
        Err(t.restart(Errcode::transaction_restart_nested))
    }

    /// @k, in @iter's btree, has to belong to an inode of the matching type:
    /// version @i, the one it resolved to. A missing inode is recreated, or
    /// copied down from a good version in an ancestor snapshot, or - if it
    /// looks deleted - @k is; one of the wrong type gets the type its keys
    /// say it has.
    fn check_key_has_inode<'t>(
        &mut self,
        t:    &TransAttempt<'_, 't>,
        iter: &BtreeIter<'t>,
        i:    Option<usize>,
        k:    BkeySC<'_>,
    ) -> Result<(), BchError> {
        let fs = t.fs();
        let btree = iter.btree();

        // whiteouts and hash whiteouts are tombstones - they need no inode:
        if k.k.key_type().is_extent_whiteout() || k.k.key_type().is_hash_whiteout() {
            return Ok(());
        }

        let inodes = self.inodes();
        // The version @k resolved to, unless that's a whiteout:
        let live = i.filter(|&i| !inodes[i].whiteout);

        if live.is_some_and(|l| btree_matches_i_mode(btree, inodes[l].inode.bi_mode)) {
            return Ok(());
        }

        let mut buf = Printbuf::new();
        match live {
            Some(l) => write!(buf, "key for wrong inode mode {:o}", inodes[l].inode.bi_mode),
            None    => write!(buf, "key in missing inode"),
        }

        let good_ancestor = inodes.iter().position(|i2| {
            !i2.whiteout &&
            snapshot::is_ancestor(t, k.k.p.snapshot, i2.inode.bi_snapshot) &&
            btree_matches_i_mode(btree, i2.inode.bi_mode)
        });
        if let Some(g) = good_ancestor {
            prt!(&mut buf, ", but found good inode in older snapshot{}\n", inodes[g].inode);
        }

        write!(buf, "\nfound keys:\n");

        let inode_pos = spos(k.k.p.inode, 0, k.k.p.snapshot);
        let nr_keys = count_inode_keys(t, inode_pos, btree, Some(&mut buf))?;
        if nr_keys == 0 {
            fs.log().err(message!("check_key_has_inode: error finding live keys in inode"));
            return Err(fs.err(Errcode::shutdown_with_errors_unfixed));
        }

        if nr_keys >= COUNT_INODE_KEYS_MAX {
            writeln!(buf, "found {nr_keys}+ keys for this inode");
        } else {
            writeln!(buf, "found {nr_keys} keys for this inode");
        }

        let lost_data = fs.btree_lost_data(Btree::inodes);
        if lost_data {
            writeln!(buf, "data was lost in inodes btree");
        }

        // Which repair: each edits a version in place
        let fix = match live {
            None => {
                let inode_looks_deleted = good_ancestor.is_some() && nr_keys < 3 && !lost_data;
                if inode_looks_deleted {
                    writeln!(buf, "inode was deleted, will delete key");
                }

                if !inode_fsck_err!(t, k.k.p, key_in_missing_inode, "{buf}")? {
                    return Ok(());
                }

                if inode_looks_deleted {
                    return delete_stray_key(t, iter);
                }

                let Some(g) = good_ancestor else {
                    reconstruct_inode(t, btree, k.k.p.snapshot, k.k.p.inode, None)?;
                    t.commit(None, CommitFlags::NO_ENOSPC)?;
                    return Err(t.restart(Errcode::transaction_restart_commit));
                };

                // A good ancestor is visible from @k, so the walk resolved @k
                // to a version - a whiteout, there being no live one.
                // XXX: which stays marked a whiteout - see check_extents
                let i = i.expect("a good ancestor is visible, so @k resolved to a version");
                let inodes = self.inodes_mut();
                let snapshot = inodes[i].inode.bi_snapshot;
                inodes[i].inode = inodes[g].inode;
                inodes[i].inode.bi_snapshot = snapshot;
                i
            }
            Some(l) => {
                // Which is wrong, the inode's type or @k? If the inode has
                // keys of its own type - in the btree its mode owns, if any -
                // its type is right and @k is a stray. If it has none, its
                // keys say what it is:
                let mode = self.inodes()[l].inode.bi_mode;
                let own_btree = [Btree::extents, Btree::dirents]
                    .into_iter()
                    .find(|&b| btree_matches_i_mode(b, mode));
                let has_own_keys = match own_btree {
                    Some(b) => count_inode_keys(t, inode_pos, b, None)? != 0,
                    None    => false,
                };

                if has_own_keys {
                    writeln!(buf, "inode has keys of its own type: deleting this one");
                } else {
                    writeln!(buf, "inode has no keys of its own type: changing its type to match");
                }

                if !inode_fsck_err!(t, k.k.p, key_in_wrong_inode_type, "{buf}")? {
                    return Ok(());
                }

                if has_own_keys {
                    return delete_stray_key(t, iter);
                }

                // A file's i_size has to cover its data, or the extents are
                // past the end and go next: as a directory, it had none
                let (ty, size) = if btree == Btree::dirents {
                    (S_IFDIR, 0)
                } else {
                    (S_IFREG, extents_end(t, k.k.p.inode, k.k.p.snapshot)?)
                };

                let inode = &mut self.inodes_mut()[l].inode;
                inode.bi_mode = (inode.bi_mode & !S_IFMT) | ty;
                inode.bi_size = inode.bi_size.max(size);
                self.changed_inode_type = true;
                l
            }
        };

        // fsck_write(), not the self-committing write: inside the caller's commit
        // loop that eats restarts, and leaks the advanced restart_count to the caller
        inode::fsck_write(t, &mut self.inodes_mut()[fix].inode)?;
        t.commit_lazy(None, CommitFlags::NO_ENOSPC)
    }

    /// Refetch on the next walk and recount the per-inode sums: the inodes
    /// changed without a commit, or the walk is retrying. Spelled as a commit: the
    /// walker refetches when commit_count isn't the one it recorded.
    pub fn invalidate(&mut self) {
        self.commit_count = self.commit_count.wrapping_sub(1);
    }

    /// The versions visible to a key in @snapshot: its own or its ancestors', not
    /// overwritten by a version in a descendant @s has seen.
    pub fn visible_mut<'a>(
        &'a mut self,
        t:        &'a TransAttempt<'_, '_>,
        s:        &'a SnapshotsSeen,
        snapshot: u32,
    ) -> impl Iterator<Item = &'a mut WalkerEntry> {
        self.inodes.iter_mut()
            .take_while(move |i| i.inode.bi_snapshot <= snapshot)
            .filter(move |i| s.key_visible(t, i.inode.bi_snapshot, snapshot))
    }

    /// Load the versions of inode @inum that a reference from @s's position
    /// names - a dirent's target, not a walk: those visible from @s's snapshot,
    /// newest first, and in deletes() the snapshots where a whiteout hides older
    /// versions.
    pub fn get_visible(
        &mut self,
        t:     &TransAttempt<'_, '_>,
        s:     &SnapshotsSeen,
        inum:  u64,
    ) -> Result<(), BchError> {
        let fs = t.fs();
        let snapshot = s.pos.snapshot;

        self.inodes.clear();
        self.deletes.clear();

        let (inodes, deletes) = (&mut self.inodes, &mut self.deletes);
        let mut iter = BtreeIter::new(t, Btree::inodes, spos(0, inum, snapshot),
                                      BtreeIterFlags::ALL_SNAPSHOTS);
        iter.for_each_reverse_norestart(t, pos(0, inum), |_, k| {
            let id = k.k.p.snapshot;

            if !s.ref_visible(t, snapshot, id) ||
               deletes.iter().any(|&d| snapshot::is_ancestor(t, id, d)) {
                return Ok(());
            }

            if k.k.is_inode() {
                inodes.push(WalkerEntry::new(fs, k))?;
            } else {
                deletes.push(id)?;
            }
            Ok(())
        })?;

        // We scan @snapshot and its descendants. With no version at @snapshot, it
        // sees its nearest ancestor's - only that one, the rest shadowed by it.
        // Normally there is one, since what makes a dirent writes its inode there;
        // this is for what repair or corruption left. First, as the oldest, and
        // shared with the ancestor's other descendants: writers use own_version().
        if !self.inodes.iter().any(|i| i.inode.bi_snapshot == snapshot) &&
           !self.deletes.contains(&snapshot) {
            if let Some(u) = inode::find_by_inum_snapshot(t, inum, snapshot,
                                                          BtreeIterFlags::empty()).found()? {
                if u.bi_snapshot != snapshot {
                    let e = WalkerEntry { inode: u, whiteout: false, count: 0 };
                    self.inodes.insert(0, e)?;
                }
            }
        }
        Ok(())
    }

    pub fn inodes(&self) -> &[WalkerEntry] {
        &self.inodes
    }

    pub fn inodes_mut(&mut self) -> &mut [WalkerEntry] {
        &mut self.inodes
    }

    /// From get_visible(): snapshots where the inode was deleted.
    pub fn deletes(&self) -> &[u32] {
        &self.deletes
    }

    /// The inode whose versions walk() has loaded, if any.
    pub fn cur_inum(&self) -> Option<u64> {
        self.have_inodes.then_some(self.last_pos.inode)
    }

    /// The versions were refetched partway through the inode, so its per-pass
    /// counts are partial: recount them rather than reporting a mismatch.
    pub fn recalculate_sums(&self) -> bool {
        self.recalculate_sums
    }

    /// A repair changed what this inode's keys add up to: recount at the end
    /// of the inode instead of reporting the mismatch.
    pub fn set_recalculate_sums(&mut self) {
        self.recalculate_sums = true;
    }

    /// Whether a repair has changed an inode's type since the last call. The
    /// dirent pointing at it may already have been checked - its d_type, and
    /// its directory's subdirectory count - under the old one.
    pub fn take_changed_inode_type(&mut self) -> bool {
        core::mem::take(&mut self.changed_inode_type)
    }
}

impl Default for InodeWalker {
    fn default() -> Self {
        Self::new()
    }
}

/// Delete @iter's stray key, commit, and restart: the per-key checks carry on
/// with the key they were handed, and a deleted dirent would be rehashed back
/// in. With an empty disk reservation: an extents update needs one, and a
/// deletion charges nothing.
fn delete_stray_key<'t>(t: &TransAttempt<'_, 't>, iter: &BtreeIter<'t>) -> Result<(), BchError> {
    let res = DiskReservation::new(t.fs());
    t.delete_at(iter, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
    t.commit(Some(&res), CommitFlags::NO_ENOSPC)?;
    Err(t.restart(Errcode::transaction_restart_commit))
}

/// Where @inum's last extent, as @snapshot sees it, ends, in bytes: the
/// i_size its data needs.
fn extents_end(t: &TransAttempt<'_, '_>, inum: u64, snapshot: u32) -> Result<u64, BchError> {
    let mut iter = BtreeIter::new(t, Btree::extents,
                                  spos(inum, u64::MAX, snapshot), BtreeIterFlags::empty());
    Ok(iter.peek_prev_min(t, pos(inum, 0))?.map_or(0, |k| k.k.p.offset << 9))
}

/// The inode mode for dirent type @d_type, if it's a file type.
fn d_type_mode(d_type: u8) -> Option<umode_t> {
    [S_IFREG, S_IFDIR, S_IFLNK, S_IFCHR, S_IFBLK, S_IFIFO, S_IFSOCK]
        .into_iter()
        .find(|m| m.d_type() == d_type)
}

/// Recreate missing inode @inum:@snapshot from the keys found in @btree -
/// a directory for dirents, a file sized to its extents - for the caller to
/// commit. Xattrs don't say what the inode was: @d_type, from a dirent that
/// names it, does if there is one; a regular file if not.
pub fn reconstruct_inode(
    t:        &TransAttempt<'_, '_>,
    btree:    Btree,
    snapshot: u32,
    inum:     u64,
    d_type:   Option<u8>,
) -> Result<(), BchError> {
    let fs = t.fs();

    let (mode, size) = match btree {
        // may race with repair deleting the extents that triggered us
        Btree::extents => (S_IFREG, extents_end(t, inum, snapshot)?),
        Btree::dirents => (S_IFDIR, 0),
        Btree::xattrs  => (d_type.and_then(d_type_mode).unwrap_or(S_IFREG), 0),
        _ => unreachable!("reconstruct_inode() for btree {}", { btree.0 }),
    };

    let mut new = inode::init(fs, 0, 0, mode | umode_t(0o600), dev_t::NONE, None);
    new.bi_size     = size;
    new.bi_inum     = inum;
    new.bi_snapshot = snapshot;

    // Recover the hash info from any surviving version: every version shares
    // bi_hash_seed and the str_hash type (repair_inode_hash_info() enforces it),
    // so a descendant will do. Node loss can take one snapshot's inode and leave
    // another's; a random seed would put every existing dirent at the wrong
    // offset, and the directory quietly becomes untraversable.
    let hash_src = match inode::find_oldest_snapshot(t, inum, snapshot) {
        Err(e) if e.matches(c::ENOENT) => inode::find_any_snapshot(t, inum),
        r => r,
    }.found()?;
    if let Some(src) = hash_src {
        new.bi_hash_seed = src.bi_hash_seed;
        new.set_str_hash(src.str_hash());
    }

    inode::fsck_write(t, &mut new)
}

/// The root inode for recreating subvolume @subvol at @snapshot: @inum if the
/// caller knows it, else found. fsck_repair_unimplemented if it can't be,
/// decided before anything is queued, so a caller can fall back to reporting.
/// @snapshot must be a leaf: the subvolume key sets its subvol backref, which
/// The snapshot key's validate rejects on a node with children.
pub fn reconstruct_subvol_root(
    t:        &TransAttempt<'_, '_>,
    snapshot: u32,
    subvol:   u32,
    inum:     Option<u64>,
) -> Result<u64, BchError> {
    let fs = t.fs();

    if !fs.snapshots.is_leaf(snapshot)? {
        fs.log().err(message!(
            "need to reconstruct subvol {subvol}, but snapshot {snapshot} is an interior node"));
        return Err(fs.err(Errcode::fsck_repair_unimplemented));
    }

    if let Some(inum) = inum {
        return Ok(inum);
    }

    // Find the root, don't mint one: when the subvolume key is what went missing,
    // the inode with bi_subvol == @subvol is still there, and a second root would
    // orphan the real contents. Nor can it wait for a later pass: a subvolume key
    // with inode == 0 is rejected (subvol_inode_bad).
    let mut iter = BtreeIter::new(t, Btree::inodes, pos(0, 0),
                                  BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);
    let root = iter.for_each_norestart(t, |_, k| Ok(
        match k.arm::<inode::Inodes>().map(|k| inode::unpack(fs, k)) {
            Some(u) if u.bi_subvol == subvol => ControlFlow::Break(Some(u.bi_inum)),
            _ => ControlFlow::Continue(()),
        }))?;

    root.ok_or_else(|| {
        fs.log().err(message!("no root inode found for subvol {subvol}, can't reconstruct"));
        fs.err(Errcode::fsck_repair_unimplemented)
    })
}

/// The subvolume leaf @snapshot was made for, by its root: the inode there
/// naming a subvolume - a new subvolume or snapshot writes its root's version
/// at its own leaf - as (subvolume, inum).
pub fn subvol_root_at(t: &TransAttempt<'_, '_>, snapshot: u32) -> Result<Option<(u32, u64)>, BchError> {
    let fs = t.fs();
    let mut iter = BtreeIter::new(t, Btree::inodes, pos(0, 0),
                                  BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);
    iter.for_each_norestart(t, |_, k| Ok(
        match k.arm::<inode::Inodes>().filter(|k| k.k.p.snapshot == snapshot).map(|k| inode::unpack(fs, k)) {
            Some(u) if u.bi_subvol != 0 => ControlFlow::Break(Some((u.bi_subvol, u.bi_inum))),
            _ => ControlFlow::Continue(()),
        }))
}

/// Where root inode @root, at @snapshot, says its subvolume is mounted.
fn root_fs_path_parent(t: &TransAttempt<'_, '_>, snapshot: u32, root: u64) -> Result<u32, BchError> {
    match inode::find_by_inum_snapshot(t, root, snapshot, BtreeIterFlags::empty()) {
        Ok(u)                          => Ok(u.bi_parent_subvol),
        Err(e) if e.matches(c::ENOENT) => Ok(0),
        Err(e)                         => Err(e),
    }
}

/// Point subvolume @subvol at leaf @snapshot, which backrefs it, with root
/// inode @root; the rest of its key - otime, creation_parent, ro - stays.
/// Where it's mounted, and whether it's a snapshot, as reconstruct_subvol()
/// has them.
pub fn repoint_subvol(
    t:        &TransAttempt<'_, '_>,
    snapshot: u32,
    subvol:   u32,
    root:     u64,
) -> Result<(), BchError> {
    let fs = t.fs();

    let fs_path_parent = root_fs_path_parent(t, snapshot, root)?;
    let master = snapshot::tree_lookup(t, fs.snapshots.tree(snapshot))?.master_subvol.get();

    fs.log().err_msg(
        t.bkey_get_mut::<Subvolume, _>(Btree::subvolumes, pos(0, subvol as u64),
                                        BtreeIterFlags::CACHED, UpdateTriggerFlags::empty(), |s| {
            let v = s.v_mut();
            v.snapshot       = snapshot.into();
            v.inode          = root.into();
            v.fs_path_parent = fs_path_parent.into();
            v.set_snap(master != 0 && master != subvol);
            Ok(())
        }),
        message!("getting subvolume {subvol}"))
}

/// Recreate missing subvolume key @subvol, at leaf @snapshot with root inode
/// @root - from reconstruct_subvol_root() - and point the snapshot, and its
/// tree if it has no master subvolume, back at it. Where it's mounted is the
/// root's to say; it's a snapshot if its tree has a master already.
pub fn reconstruct_subvol(
    t:        &TransAttempt<'_, '_>,
    snapshot: u32,
    subvol:   u32,
    root:     u64,
) -> Result<(), BchError> {
    let fs = t.fs();

    fs.log().info(message!("reconstructing subvol {subvol} with root inode {root}"));

    let fs_path_parent = root_fs_path_parent(t, snapshot, root)?;

    let tree = fs.log().err_msg(
        t.bkey_get_mut::<Snapshot, _>(Btree::snapshots, pos(0, snapshot as u64),
                                       BtreeIterFlags::empty(), UpdateTriggerFlags::empty(), |s| {
            let s = s.v_mut();
            s.subvol = subvol.into();
            s.set_state(SnapshotState::live);
            Ok(s.tree())
        }),
        message!("getting snapshot {snapshot}"))?;

    let had_master = fs.log().err_msg(
        t.bkey_get_mut::<SnapshotTree, _>(Btree::snapshot_trees, pos(0, tree as u64),
                                           BtreeIterFlags::empty(), UpdateTriggerFlags::empty(), |st| {
            let st = st.v_mut();
            let had_master = st.master_subvol != 0;
            if !had_master {
                st.master_subvol = subvol.into();
            }
            Ok(had_master)
        }),
        message!("getting snapshot tree {tree}"))?;

    let mut k = t.bkey_alloc_typed::<Subvolume>()?;
    k.k_mut().p = pos(0, subvol as u64);
    let v = k.v_mut();
    v.snapshot       = snapshot.into();
    v.inode          = root.into();
    v.fs_path_parent = fs_path_parent.into();
    v.set_snap(had_master);
    v.set_state(SubvolumeState::live);
    t.insert(Btree::subvolumes, k, UpdateTriggerFlags::empty())
}

/// Whether an inode of mode @mode can own keys in @btree.
fn btree_matches_i_mode(btree: Btree, mode: umode_t) -> bool {
    match btree {
        Btree::extents => mode.is_reg() || mode.is_lnk(),
        Btree::dirents => mode.is_dir(),
        Btree::xattrs  => true,
        _ => unreachable!("check_key_has_inode() on btree {}", { btree.0 }),
    }
}

/// Where count_inode_keys() stops counting.
const COUNT_INODE_KEYS_MAX: u32 = 100;

/// How many keys inode @inode_pos has in @btree, as its version in
/// @inode_pos's snapshot sees them - up to COUNT_INODE_KEYS_MAX - printing
/// the first ten to @out.
fn count_inode_keys(
    t:         &TransAttempt<'_, '_>,
    inode_pos: Bpos,
    btree:     Btree,
    mut out:   Option<&mut Printbuf>,
) -> Result<u32, BchError> {
    let fs = t.fs();
    let mut nr_keys = 0;
    let mut iter = BtreeIter::new(t, btree, inode_pos, BtreeIterFlags::TRIM);

    iter.for_each_max_norestart(t, pos(inode_pos.inode, u64::MAX), |_, k| {
        // Error keys count: they're placeholders for unreadable data,
        // evidence the inode had contents. Hash whiteouts are just
        // tombstones:
        if k.k.key_type().is_hash_whiteout() {
            return Ok(ControlFlow::Continue(()));
        }

        nr_keys += 1;
        if let Some(out) = out.as_deref_mut().filter(|_| nr_keys <= 10) {
            prt!(out, "{}\n", k.to_text(fs));
        }

        Ok(if nr_keys >= COUNT_INODE_KEYS_MAX { ControlFlow::Break(()) } else { ControlFlow::Continue(()) })
    })?;

    Ok(nr_keys)
}

/// A repair to @inode made for a key at @snapshot: if @inode is a version in
/// a strict ancestor - one get_visible() resolved @snapshot to - it's shared
/// with the ancestor's other descendants, and the repair goes in @snapshot
/// instead, as a version of its own. Written in place, it would change what
/// every other view of the ancestor sees.
pub fn own_version(t: &TransAttempt<'_, '_>, inode: &mut InodeUnpacked, snapshot: u32) {
    if inode.bi_snapshot != snapshot && snapshot::is_ancestor(t, snapshot, inode.bi_snapshot) {
        inode.bi_snapshot = snapshot;
    }
}

/// @new, a key a hash table repair just wrote, may be a dirent at a new
/// position: point what it names back at it - a subvolume's root inode, or
/// the versions of an inode visible from it, by @s.
pub fn fsck_update_backpointers(
    t:   &TransAttempt<'_, '_>,
    s:   &mut SnapshotsSeen,
    new: &BkeyI,
) -> Result<(), BchError> {
    let Some(d) = Dirent::new(BkeySC::from(new)) else { return Ok(()) };
    let (dir, offset) = (new.k.p.inode, new.k.p.offset);
    let points_here = |i: &InodeUnpacked| i.bi_dir == dir && i.bi_dir_offset == offset;

    match d.target() {
        // A subvolume dirent's backpointer lives on the child subvolume's root
        // inode, same as a regular inode - see inode_get_dirent(). A
        // dangling subvol dirent (subvol or root inode gone) is
        // check_subvols/check_dirents' problem, not ours.
        DirentTarget::Subvol { child, .. } => {
            let Some(subvol) = subvolume::get(t, child, false).found()? else { return Ok(()) };
            let Some(mut root) = inode::find_by_inum_snapshot(t, subvol.inode.get(),
                                                              subvol.snapshot.get(),
                                                              BtreeIterFlags::empty()).found()? else {
                return Ok(());
            };
            if points_here(&root) {
                return Ok(());
            }

            root.bi_dir        = dir;
            root.bi_dir_offset = offset;
            inode::fsck_write(t, &mut root)
        }
        DirentTarget::Inode(inum) => {
            let mut target = InodeWalker::new();
            target.get_visible(t, s, inum)?;

            // A backpointer is the (bi_dir, bi_dir_offset) pair: compare and set both, or
            // a matching offset in the wrong directory skips a broken one. Skip before
            // writing: fsck_write() allocates trans mem per call, once per visible
            // version, so a correct backpointer must cost nothing - and a re-run over
            // partly repaired state then shrinks instead of repeating.
            for i in target.inodes_mut().iter_mut().filter(|i| !points_here(&i.inode)) {
                own_version(t, &mut i.inode, new.k.p.snapshot);
                i.inode.bi_dir        = dir;
                i.inode.bi_dir_offset = offset;
                inode::fsck_write(t, &mut i.inode)?;
            }
            Ok(())
        }
    }
}

impl Default for SnapshotsSeen {
    fn default() -> Self {
        Self::new()
    }
}

impl Fs {
    /// fsck(8)'s exit status for what the filesystem's errors came to - 1
    /// fixed, 4 left, 8 a fatal error, ORed - each said in @out.
    pub fn fsck_errcode(&self, out: &mut Printbuf) -> i32 {
        use crate::fs::FsFlag as Flag;

        let mut ret = 0;
        for (flag, bit, what) in [
            (Flag::errors_fixed, 1, "errors fixed"),
            (Flag::error,        4, "still has errors"),
            (Flag::emergency_ro, 8, "fatal error (went emergency read-only)"),
        ] {
            if self.flags.get(flag) {
                out.write_bytes(self.name().to_bytes());
                prt!(out, ": {what}\n");
                ret |= bit;
            }
        }
        ret
    }
}

/// BCH_IOCTL_FSCK_OFFLINE and BCH_IOCTL_FSCK_ONLINE: fsck in the kernel, for
/// the bcachefs fsck command. Each runs in a kthread behind a stdio file, its
/// output read from the fd and its questions answered by writing to it; what
/// it returns - an fsck(8) exit status, or an error - is what closing the fd
/// returns.
#[cfg(kernel)]
pub mod ioctl {
    use super::*;
    use crate::bcachefs_ioctl::{IoctlFsckOffline, IoctlFsckOnline};
    use crate::format::ErrorAction;
    use crate::fs::FsFlag;
    use crate::opts::{Opts, FsckErrOpt, OptId};
    use crate::fs::{OpenFs, RoRef};
    use crate::init::registry::{self, Claim, FsHandle, Opener};
    use crate::init::passes::{passes_match, run_passes, PassFlags, RecoveryPass};
    use crate::opts::parse_mount_opts_cstr;
    use crate::os::kernel::{capable, Capability};
    use crate::util::refcount::TryRefcount;
    use crate::util::{alloc::Arc, log::{print_str, Log, LogLevel, Only}, names::set, thread_with_file::{Reader, StdioRedirect, ThreadWithFile}};
    use crate::os::ioctl::{UserStr, UserVstruct};

    /// The options at user address @addr, or none if it's 0.
    fn user_opts(addr: u64) -> Result<Opts, BchError> {
        match addr {
            0 => Ok(Opts::default()),
            _ => parse_mount_opts_cstr(None, UserStr::read(addr, 1 << 16)?.as_cstr(), false),
        }
    }

    pub fn fsck_offline(user: UserVstruct<IoctlFsckOffline>) -> Result<i32, BchError> {
        let arg = user.head()?;

        if arg.flags != 0 {
            return Err(Errcode::EINVAL_fsck_offline_bad_flags.into());
        }

        if !capable(Capability::SysAdmin) {
            return Err(Errcode::EPERM_fsck_offline_not_admin.into());
        }

        let mut opts = user_opts(arg.opts)?;

        let user_devs = user.tail(&arg);
        let mut devs = Vec::with_capacity(user_devs.len())?;
        for i in 0..user_devs.len() {
            devs.push(UserStr::read(user_devs.get(i)?, c::PATH_MAX as usize)?)?;
        }

        let thr = ThreadWithFile::new_stdio(Ok(()), Reader::Waited)?;

        opts.set_read_only(true);
        opts.set_ratelimit_errors(false);
        // We need request_key() to be called before we punt to kthread:
        opts.set_nostart(true);

        // held until it's open: a mount meanwhile gets EBUSY
        let fs = registry::devs(devs.iter().map(UserStr::as_cstr))
            .and_then(|d| FsHandle::get(&d, Opener::Fsck))
            .and_then(|handle| {
                let FsHandle::Opening(opening) = handle else { return Err(Errcode::EBUSY_fsck_offline_opening.into()) };
                let Claim::Ours(_opening, claimed) = opening.claim(devs.iter().map(UserStr::as_cstr), &mut opts)? else {
                    return Err(Errcode::EBUSY_fsck_offline_claim.into());
                };
                claimed.open_keeping(&mut opts, None)
            });

        if let Ok((fs, _)) = &fs {
            if fs.opts.errors() == ErrorAction::panic {
                fs.opts_writer(&mut fs.sb_lock()).set_errors(ErrorAction::ro);
            }
        }

        thr.run_stdio(move |(), stdio| fsck_offline_thread(fs, stdio))
            .inspect_err(|e| Log::NO_FS.err(message!("fsck_offline: error starting fsck thread: {}", e.msg())))
    }

    /// The fsck(8) exit status, and what it means for whoever's at the redirect.
    fn fsck_result(fs: &Fs) -> i32 {
        let mut buf = Printbuf::new();
        let ret = fs.fsck_errcode(&mut buf);
        print_str(fs, LogLevel::ERR, &mut buf);
        ret
    }

    fn fsck_offline_thread(fs: Result<(OpenFs, Printbuf), BchError>, stdio: &Arc<StdioRedirect>) -> i32 {
        let (fs, opened) = match fs {
            Ok(fs) => fs,
            Err(e) => return -e.raw(),
        };

        // the filesystem is fsck's alone: everything it says is for its user
        let attached = fs.stdio.attach(stdio, Only::All);
        if !opened.as_bytes().is_empty() {
            fs.log().info(&opened);
        }

        fs.recovery_task.store(crate::os::sched::current(), core::sync::atomic::Ordering::Relaxed);

        let ret = match fs.start() {
            Err(e) => {
                fs.log().err(message!("error starting filesystem: {}", e.msg()));
                // What we return is an fsck(8) exit status, not an errcode -
                // see Fs::fsck_errcode(). A filesystem we couldn't start
                // is an operational error, same as the online path reports
                // for recovery passes that fail outright.
                8
            }
            Ok(()) => fsck_result(&fs),
        };

        // shutting down attached, so what it says reaches the user too; the
        // guard's borrow of the filesystem ends before exit() takes it
        let _ = fs.stop();
        drop(attached);
        let _ = fs.exit();
        ret
    }

    pub fn fsck_online(fs: &Fs, arg: IoctlFsckOnline) -> Result<i32, BchError> {
        if arg.flags != 0 {
            return fs.throw(Errcode::EINVAL_fsck_online_bad_flags);
        }

        if !capable(Capability::SysAdmin) {
            return fs.throw(Errcode::EPERM_non_admin);
        }

        let opts = user_opts(arg.opts)?;

        let fs_ref = fs.require(fs.ro_ref.tryget(), Errcode::erofs_fsck_online_no_ro_ref)?;

        let thr = ThreadWithFile::new_stdio(Ok(()), Reader::Waited)?;
        fs.log().err_fn(thr.run_stdio(move |(), stdio| fsck_online_thread(fs_ref, opts, stdio)))
    }

    fn fsck_online_thread(fs_ref: RoRef, opts: Opts, stdio: &Arc<StdioRedirect>) -> i32 {
        let fs: &Fs = &fs_ref;

        // Another fsck is running, attached: this one tells its user itself.
        let Some(_run) = fs.recovery.run_lock.try_lock() else {
            let mut buf = Printbuf::new();
            prt!(&mut buf, "{}: error running recovery passes: {}\n", fs.name().to_str().unwrap_or("?"),
                 BchError::from(Errcode::EAGAIN_fsck_online_recovery_running).msg());
            let _ = stdio.write(buf.as_bytes());
            return 8;
        };
        let _attached = fs.stdio.attach(stdio, Only::Task);

        let online = passes_match(PassFlags::ONLINE);
        let mut passes = passes_match(PassFlags::FSCK) & online;

        if opts.defined(OptId::recovery_passes) {
            passes = opts.recovery_passes;

            if passes & online != passes {
                fs.log().err(message!("Cannot run passes {} online", set::<RecoveryPass>(passes & !online)));
                return -fs.err(Errcode::EINVAL_fsck_online_bad_passes).raw();
            }
        }

        // XXX: can we figure out a way to do this without mucking with c->opts?
        let old_fix_errors = fs.opts.fix_errors.get();
        {
            let mut sb = fs.sb_lock();
            let w = fs.opts_writer(&mut sb);
            w.fix_errors.set(if opts.defined(OptId::fix_errors) {
                opts.fix_errors
            } else {
                FsckErrOpt::ask as u8
            });
            w.fsck.set(1);
        }
        fs.flags.set(FsFlag::in_fsck, true);

        let ret = run_passes(&fs, passes, true);

        fs.flags.set(FsFlag::in_fsck, false);
        fs.opts_writer(&mut fs.sb_lock()).fix_errors.set(old_fix_errors);

        match ret {
            Ok(()) => fsck_result(fs),
            Err(e) => {
                fs.log().err(message!("error running recovery passes: {}", e.msg()));
                8
            }
        }
    }
}
