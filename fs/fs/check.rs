// SPDX-License-Identifier: GPL-2.0

//! Helpers shared by the fsck passes.

use crate::btree::bkey::{bkey_extent_whiteout, pos, spos, BkeyI, BkeySC};
use crate::btree::iter::{
    BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, TransAttempt, UpdateTriggerFlags,
};
use crate::c;
use crate::errcode::{bch_errcode, BchError};
use crate::fs::Fs;
use crate::init::error::id;
use crate::inode;
use crate::dirent::{Dirent, DirentTarget};
use crate::errcode::Found;
use crate::snapshots::{snapshot, subvolume};
use crate::alloc::buckets::DiskReservation;
use crate::util::alloc::{flags::GFP_KERNEL, kvvec_insert, KVVec};
use crate::util::Printbuf;
use crate::{bch_err, bch_err_msg, bch_info, fsck_err_on, inode_fsck_err};
use core::ops::ControlFlow;

/// The snapshots of the keys seen so far at one position, while walking a
/// btree in key order with all snapshots: a key in an ancestor is hidden from
/// a descendant that overwrote it. C only passes it through, from check_key()
/// back to __bch2_str_hash_check_key().
pub struct SnapshotsSeen {
    pos: c::bpos,
    /// Ascending, as seen in key order, plus any a repair added in order.
    ids: KVVec<u32>,
}

impl SnapshotsSeen {
    pub fn new() -> Self {
        SnapshotsSeen { pos: Default::default(), ids: KVVec::new() }
    }

    /// Record a key at @pos; a new inode:offset starts the list over.
    pub fn update(&mut self, pos: c::bpos) -> Result<(), BchError> {
        if self.pos.inode != pos.inode || self.pos.offset != pos.offset {
            self.ids.clear();
        }
        self.pos = pos;

        if !self.has_id(pos.snapshot) {
            self.ids.push(pos.snapshot, GFP_KERNEL)?;
        }
        Ok(())
    }

    /// Whether a key in @ancestor is visible in its descendant @id: not
    /// overwritten by a key this list has seen.
    pub fn key_visible(&self, trans: &BtreeTrans<'_>, id: u32, ancestor: u32) -> bool {
        debug_assert!(id <= ancestor);

        if id == ancestor {
            return true;
        }
        if !snapshot::is_ancestor(trans, id, ancestor) {
            return false;
        }

        // Overwritten by a key seen in a snapshot between the two?
        !self.ids.iter().any(|&i| i != ancestor && snapshot::is_ancestor(trans, id, i))
    }

    /// Whether a key in @src points at something visible as a key in @dst, in
    /// some snapshot. The @src keys must be walked in key order, recorded here.
    pub fn ref_visible(&self, trans: &BtreeTrans<'_>, src: u32, dst: u32) -> bool {
        if dst <= src {
            self.key_visible(trans, dst, src)
        } else {
            snapshot::is_ancestor(trans, src, dst)
        }
    }

    /// The snapshots that have overwritten the key at @pos in @btree, as if a walk
    /// had seen them: for a key checked out of walk order.
    pub fn overwrites(t: &TransAttempt<'_, '_>, btree: c::btree_id, pos: c::bpos)
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
        let mut ids = KVVec::with_capacity(self.ids.len(), GFP_KERNEL)?;
        for &i in self.ids.iter() {
            ids.push(i, GFP_KERNEL)?;
        }
        Ok(SnapshotsSeen { pos: self.pos, ids })
    }

    /// For a key a repair created at this position: kept sorted.
    pub fn add_inorder(&mut self, id: u32) -> Result<(), BchError> {
        match self.ids.iter().position(|&i| i >= id) {
            Some(idx) if self.ids[idx] == id => {}
            Some(idx) => kvvec_insert(&mut self.ids, idx, id)?,
            None => self.ids.push(id, GFP_KERNEL)?,
        }
        Ok(())
    }
}

/// Whether a key in @src and one in @dst are both visible in some snapshot:
/// one an ancestor of the other, not overwritten in between.
pub fn ref_visible2(
    trans:    &BtreeTrans<'_>,
    src:      u32,
    src_seen: &SnapshotsSeen,
    dst:      u32,
    dst_seen: &SnapshotsSeen,
) -> bool {
    if dst > src {
        dst_seen.key_visible(trans, src, dst)
    } else {
        src_seen.key_visible(trans, dst, src)
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
    last_pos:         c::bpos,
    /// The versions are valid while the transaction's commit_count is this.
    commit_count:     u32,
    inodes:           KVVec<WalkerEntry>,
    /// From get_visible(): snapshots where a whiteout hides older versions.
    deletes:          KVVec<u32>,
    /// A repair changed an inode's type: see take_changed_inode_type().
    changed_inode_type: bool,
}

/// One version of the walked inode - or a whiteout, where the inode was
/// deleted in that snapshot - and what the pass has counted against it.
#[derive(Clone, Copy)]
pub struct WalkerEntry {
    pub inode:    c::bch_inode_unpacked,
    pub whiteout: bool,
    pub count:    u64,
}

impl WalkerEntry {
    fn new(fs: &Fs, k: BkeySC<'_>) -> Self {
        let whiteout = !inode::bkey_is_inode(k.k);
        let inode = if whiteout {
            c::bch_inode_unpacked {
                bi_inum:     k.k.p.offset,
                bi_snapshot: k.k.p.snapshot,
                ..Default::default()
            }
        } else {
            inode::unpack(fs, k)
        };
        WalkerEntry { inode, whiteout, count: 0 }
    }
}

/// The live inode version a key belongs to, from InodeWalker::walk().
pub struct Walked<'w> {
    pub inode:            &'w mut c::bch_inode_unpacked,
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
            inodes:           KVVec::new(),
            deletes:          KVVec::new(),
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
        let trans = t.trans();

        if self.last_pos.inode != k.k.p.inode {
            self.get_inodes_all_snapshots(t, k.k.p.inode)?;
        } else if self.commit_count != trans.commit_count() {
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
        let trans: &BtreeTrans<'_> = t;
        let fs = trans.fs();

        // No inodes for last_pos now: clear it, or a restart here would confuse
        // check_i_sectors and check_subdir_count
        self.have_inodes = false;
        self.recalculate_sums = false;
        self.inodes.clear();

        let inodes = &mut self.inodes;
        let mut iter = BtreeIter::new(trans, c::btree_id::inodes, pos(0, inum),
                                      BtreeIterFlags::ALL_SNAPSHOTS);
        iter.for_each_max_norestart(t, spos(0, inum, u32::MAX), |_, k| {
            inodes.push(WalkerEntry::new(fs, k), GFP_KERNEL)?;
            Ok(())
        })?;

        self.first_this_inode = true;
        self.have_inodes = true;
        self.commit_count = trans.commit_count();
        Ok(())
    }

    /// The version @k belongs to: the nearest, in its snapshot or an ancestor. It
    /// should be @k's own, a key's inode being updated with it; if it's an
    /// ancestor's, the inode (or whiteout) is copied into @k's snapshot,
    /// committed, and a restart returned.
    fn lookup_inode_for_snapshot(&mut self, t: &TransAttempt<'_, '_>, k: BkeySC<'_>)
        -> Result<Option<usize>, BchError>
    {
        let trans = t.trans();
        let fs = trans.fs();
        let snapshot = k.k.p.snapshot;
        let k_snapshot = fs.snapshots.redundant_interior(snapshot).unwrap_or(snapshot);

        let Some(i) = self.inodes.iter()
            .position(|i| snapshot::is_ancestor(trans, k_snapshot, i.inode.bi_snapshot)) else {
            return Ok(None);
        };

        let e = self.inodes[i];
        let inode_snapshot = fs.snapshots.redundant_interior(e.inode.bi_snapshot)
            .unwrap_or(e.inode.bi_snapshot);

        if !fsck_err_on!(trans, k_snapshot != inode_snapshot, id::snapshot_key_missing_inode_snapshot,
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
            let whiteout = t.bkey_alloc_init(0, c::bch_bkey_type::KEY_TYPE_whiteout.0 as u8,
                                             spos(0, e.inode.bi_inum, snapshot))?;
            t.insert_with(c::btree_id::inodes, whiteout, BtreeIterFlags::CACHED,
                          UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
        }
        t.commit(None, CommitFlags::empty())?;

        // walk() invalidates on the restart: the retry refetches the versions,
        // the one just written included
        Err(t.restart(bch_errcode::BCH_ERR_transaction_restart_nested))
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
        let trans = t.trans();
        let fs = trans.fs();
        let btree = iter.btree();

        // whiteouts and hash whiteouts are tombstones - they need no inode:
        if bkey_extent_whiteout(k.k) || k.key_type() == c::bch_bkey_type::KEY_TYPE_hash_whiteout {
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
            snapshot::is_ancestor(trans, k.k.p.snapshot, i2.inode.bi_snapshot) &&
            btree_matches_i_mode(btree, i2.inode.bi_mode)
        });
        if let Some(g) = good_ancestor {
            writeln!(buf, ", but found good inode in older snapshot{}", inodes[g].inode);
        }

        write!(buf, "\nfound keys:\n");

        let inode_pos = spos(k.k.p.inode, 0, k.k.p.snapshot);
        let nr_keys = count_inode_keys(t, inode_pos, btree, Some(&mut buf))?;
        if nr_keys == 0 {
            bch_err!(fs, "check_key_has_inode: error finding live keys in inode");
            return Err(fs.err(bch_errcode::BCH_ERR_shutdown_with_errors_unfixed));
        }

        if nr_keys >= COUNT_INODE_KEYS_MAX {
            writeln!(buf, "found {nr_keys}+ keys for this inode");
        } else {
            writeln!(buf, "found {nr_keys} keys for this inode");
        }

        let lost_data = fs.btree_lost_data(c::btree_id::inodes);
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

                if !inode_fsck_err!(trans, k.k.p, id::key_in_missing_inode, "{buf}")? {
                    return Ok(());
                }

                if inode_looks_deleted {
                    return delete_stray_key(t, iter);
                }

                let Some(g) = good_ancestor else {
                    reconstruct_inode(t, btree, k.k.p.snapshot, k.k.p.inode, None)?;
                    t.commit(None, CommitFlags::NO_ENOSPC)?;
                    return Err(t.restart(bch_errcode::BCH_ERR_transaction_restart_commit));
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
                let own_btree = [c::btree_id::extents, c::btree_id::dirents]
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

                if !inode_fsck_err!(trans, k.k.p, id::key_in_wrong_inode_type, "{buf}")? {
                    return Ok(());
                }

                if has_own_keys {
                    return delete_stray_key(t, iter);
                }

                // A file's i_size has to cover its data, or the extents are
                // past the end and go next: as a directory, it had none
                let (ty, size) = if btree == c::btree_id::dirents {
                    (c::S_IFDIR, 0)
                } else {
                    (c::S_IFREG, extents_end(t, k.k.p.inode, k.k.p.snapshot)?)
                };

                let inode = &mut self.inodes_mut()[l].inode;
                inode.bi_mode = (inode.bi_mode & !(c::S_IFMT as c::umode_t)) | ty as c::umode_t;
                inode.bi_size = inode.bi_size.max(size);
                self.changed_inode_type = true;
                l
            }
        };

        // fsck_write(), not the self-committing write: inside the caller's commit
        // loop that eats restarts, and leaks the advanced restart_count to the caller
        inode::fsck_write(t, &mut self.inodes_mut()[fix].inode)?;
        t.commit_lazy(CommitFlags::NO_ENOSPC)
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
        trans:    &'a BtreeTrans<'_>,
        s:        &'a SnapshotsSeen,
        snapshot: u32,
    ) -> impl Iterator<Item = &'a mut WalkerEntry> {
        self.inodes.iter_mut()
            .take_while(move |i| i.inode.bi_snapshot <= snapshot)
            .filter(move |i| s.key_visible(trans, i.inode.bi_snapshot, snapshot))
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
        let trans: &BtreeTrans<'_> = t;
        let fs = trans.fs();
        let snapshot = s.pos.snapshot;

        self.inodes.clear();
        self.deletes.clear();

        let (inodes, deletes) = (&mut self.inodes, &mut self.deletes);
        let mut iter = BtreeIter::new(trans, c::btree_id::inodes, spos(0, inum, snapshot),
                                      BtreeIterFlags::ALL_SNAPSHOTS);
        iter.for_each_reverse_norestart(t, pos(0, inum), |_, k| {
            let id = k.k.p.snapshot;

            if !s.ref_visible(trans, snapshot, id) ||
               deletes.iter().any(|&d| snapshot::is_ancestor(trans, id, d)) {
                return Ok(());
            }

            if inode::bkey_is_inode(k.k) {
                inodes.push(WalkerEntry::new(fs, k), GFP_KERNEL)?;
            } else {
                deletes.push(id, GFP_KERNEL)?;
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
            if let Some(u) = inode::find_by_inum_snapshot(trans, inum, snapshot,
                                                          BtreeIterFlags::empty()).found()? {
                if u.bi_snapshot != snapshot {
                    let e = WalkerEntry { inode: u, whiteout: false, count: 0 };
                    kvvec_insert(&mut self.inodes, 0, e)?;
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
    let res = DiskReservation::new(t.trans().fs());
    t.delete_at(iter, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
    t.commit(Some(&res), CommitFlags::NO_ENOSPC)?;
    Err(t.restart(bch_errcode::BCH_ERR_transaction_restart_commit))
}

/// Where @inum's last extent, as @snapshot sees it, ends, in bytes: the
/// i_size its data needs.
fn extents_end(t: &TransAttempt<'_, '_>, inum: u64, snapshot: u32) -> Result<u64, BchError> {
    let mut iter = BtreeIter::new(t, c::btree_id::extents,
                                  spos(inum, u64::MAX, snapshot), BtreeIterFlags::empty());
    Ok(iter.peek_prev_min(t, pos(inum, 0))?.map_or(0, |k| k.k.p.offset << 9))
}

/// The inode mode for dirent type @d_type, if it's a file type.
fn d_type_mode(d_type: u8) -> Option<u32> {
    let d_type = d_type as u32;
    [c::DT_REG, c::DT_DIR, c::DT_LNK, c::DT_CHR, c::DT_BLK, c::DT_FIFO, c::DT_SOCK]
        .contains(&d_type)
        .then_some(d_type << 12)
}

/// Recreate missing inode @inum:@snapshot from the keys found in @btree -
/// a directory for dirents, a file sized to its extents - for the caller to
/// commit. Xattrs don't say what the inode was: @d_type, from a dirent that
/// names it, does if there is one; a regular file if not.
pub fn reconstruct_inode(
    t:        &TransAttempt<'_, '_>,
    btree:    c::btree_id,
    snapshot: u32,
    inum:     u64,
    d_type:   Option<u8>,
) -> Result<(), BchError> {
    let trans = t.trans();
    let fs = trans.fs();

    let (mode, size) = match btree {
        // may race with repair deleting the extents that triggered us
        c::btree_id::extents => (c::S_IFREG, extents_end(t, inum, snapshot)?),
        c::btree_id::dirents => (c::S_IFDIR, 0),
        c::btree_id::xattrs  => (d_type.and_then(d_type_mode).unwrap_or(c::S_IFREG), 0),
        _ => unreachable!("reconstruct_inode() for btree {}", { btree.0 }),
    };

    let mut new = inode::init(fs, 0, 0, (mode | 0o600) as c::umode_t, 0, None);
    new.bi_size     = size;
    new.bi_inum     = inum;
    new.bi_snapshot = snapshot;

    // Recover the hash info from any surviving version: every version shares
    // bi_hash_seed and the str_hash type (repair_inode_hash_info() enforces it),
    // so a descendant will do. Node loss can take one snapshot's inode and leave
    // another's; a random seed would put every existing dirent at the wrong
    // offset, and the directory quietly becomes untraversable.
    let hash_src = match inode::find_oldest_snapshot(trans, inum, snapshot) {
        Err(e) if e.matches(c::ENOENT) => inode::find_any_snapshot(trans, inum),
        r => r,
    }.found()?;
    if let Some(src) = hash_src {
        new.bi_hash_seed = src.bi_hash_seed;
        new.set_inode_str_hash(src.inode_str_hash());
    }

    inode::fsck_write(t, &mut new)
}

/// The root inode for recreating subvolume @subvol at @snapshot: @inum if the
/// caller knows it, else found. fsck_repair_unimplemented if it can't be,
/// decided before anything is queued, so a caller can fall back to reporting.
/// @snapshot must be a leaf: the subvolume key sets its subvol backref, which
/// bch2_snapshot_validate() rejects on a node with children.
pub fn reconstruct_subvol_root(
    t:        &TransAttempt<'_, '_>,
    snapshot: u32,
    subvol:   u32,
    inum:     Option<u64>,
) -> Result<u64, BchError> {
    let trans: &BtreeTrans<'_> = t;
    let fs = trans.fs();

    if !fs.snapshots.is_leaf(snapshot)? {
        bch_err!(fs, "need to reconstruct subvol {subvol}, but snapshot {snapshot} is an interior node");
        return Err(fs.err(bch_errcode::BCH_ERR_fsck_repair_unimplemented));
    }

    if let Some(inum) = inum {
        return Ok(inum);
    }

    // Find the root, don't mint one: when the subvolume key is what went missing,
    // the inode with bi_subvol == @subvol is still there, and a second root would
    // orphan the real contents. Nor can it wait for a later pass: a subvolume key
    // with inode == 0 is rejected (subvol_inode_bad).
    let mut iter = BtreeIter::new(trans, c::btree_id::inodes, pos(0, 0),
                                  BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);
    let root = iter.for_each_norestart(t, |_, k| Ok(
        match inode::bkey_is_inode(k.k).then(|| inode::unpack(fs, k)) {
            Some(u) if u.bi_subvol == subvol => ControlFlow::Break(Some(u.bi_inum)),
            _ => ControlFlow::Continue(()),
        }))?;

    root.ok_or_else(|| {
        bch_err!(fs, "no root inode found for subvol {subvol}, can't reconstruct");
        fs.err(bch_errcode::BCH_ERR_fsck_repair_unimplemented)
    })
}

/// Recreate missing subvolume key @subvol, at leaf @snapshot with root inode
/// @root - from reconstruct_subvol_root() - and point the snapshot, and its
/// tree if it has no master subvolume, back at it.
pub fn reconstruct_subvol(
    t:        &TransAttempt<'_, '_>,
    snapshot: u32,
    subvol:   u32,
    root:     u64,
) -> Result<(), BchError> {
    let fs = t.trans().fs();

    bch_info!(fs, "reconstructing subvol {subvol} with root inode {root}");

    let mut k = t.bkey_alloc_typed::<c::Subvolume>()?;
    k.k_mut().p = pos(0, subvol as u64);
    let v = k.v_mut();
    v.snapshot = snapshot.into();
    v.inode    = root.into();
    v.set_state(c::bch_subvolume_state::SUBVOLUME_STATE_live);
    t.insert(c::btree_id::subvolumes, k, UpdateTriggerFlags::empty())?;

    let mut s = bch_err_msg!(fs,
        t.bkey_get_mut::<c::Snapshot>(c::btree_id::snapshots, pos(0, snapshot as u64),
                                       BtreeIterFlags::empty(), UpdateTriggerFlags::empty()),
        "getting snapshot {snapshot}")?;
    let s = s.v_mut();
    let tree = s.tree();
    s.subvol = subvol.into();
    s.set_state(c::bch_snapshot_state::SNAPSHOT_STATE_live);

    let mut st = bch_err_msg!(fs,
        t.bkey_get_mut::<c::SnapshotTree>(c::btree_id::snapshot_trees, pos(0, tree as u64),
                                           BtreeIterFlags::empty(), UpdateTriggerFlags::empty()),
        "getting snapshot tree {tree}")?;
    let st = st.v_mut();
    if st.master_subvol == 0 {
        st.master_subvol = subvol.into();
    }

    Ok(())
}

/// Whether an inode of mode @mode can own keys in @btree.
fn btree_matches_i_mode(btree: c::btree_id, mode: c::umode_t) -> bool {
    let fmt = mode as u32 & c::S_IFMT;
    match btree {
        c::btree_id::extents => fmt == c::S_IFREG || fmt == c::S_IFLNK,
        c::btree_id::dirents => fmt == c::S_IFDIR,
        c::btree_id::xattrs  => true,
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
    inode_pos: c::bpos,
    btree:     c::btree_id,
    mut out:   Option<&mut Printbuf>,
) -> Result<u32, BchError> {
    let fs = t.fs();
    let mut nr_keys = 0;
    let mut iter = BtreeIter::new(t, btree, inode_pos, BtreeIterFlags::empty());

    iter.for_each_max_norestart(t, pos(inode_pos.inode, u64::MAX), |_, k| {
        // Error keys count: they're placeholders for unreadable data,
        // evidence the inode had contents. Hash whiteouts are just
        // tombstones:
        if k.key_type() == c::bch_bkey_type::KEY_TYPE_hash_whiteout {
            return Ok(ControlFlow::Continue(()));
        }

        nr_keys += 1;
        if let Some(out) = out.as_deref_mut().filter(|_| nr_keys <= 10) {
            writeln!(out, "{}", k.to_text(fs));
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
pub fn own_version(trans: &BtreeTrans<'_>, inode: &mut c::bch_inode_unpacked, snapshot: u32) {
    if inode.bi_snapshot != snapshot && snapshot::is_ancestor(trans, snapshot, inode.bi_snapshot) {
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
    let trans = t.trans();
    let Some(d) = Dirent::new(BkeySC::from(new)) else { return Ok(()) };
    let (dir, offset) = (new.k.p.inode, new.k.p.offset);
    let points_here = |i: &c::bch_inode_unpacked| i.bi_dir == dir && i.bi_dir_offset == offset;

    match d.target() {
        // A subvolume dirent's backpointer lives on the child subvolume's root
        // inode, same as a regular inode - see bch2_inode_get_dirent(). A
        // dangling subvol dirent (subvol or root inode gone) is
        // check_subvols/check_dirents' problem, not ours.
        DirentTarget::Subvol { child, .. } => {
            let Some(subvol) = subvolume::get(trans, child, false).found()? else { return Ok(()) };
            let Some(mut root) = inode::find_by_inum_snapshot(trans, subvol.inode.get(),
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
                own_version(trans, &mut i.inode, new.k.p.snapshot);
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
