// SPDX-License-Identifier: GPL-2.0

//! fsck helpers shared by the passes (fs/check.h).

use crate::btree::bkey::{bkey_extent_whiteout, pos, spos, BkeySC};
use crate::btree::iter::{
    BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, TransAttempt, TransResult, TransRet,
    UpdateTriggerFlags,
};
use crate::c;
use crate::errcode::{bch_errcode, BchError};
use crate::fs::Fs;
use crate::init::error::id;
use crate::inode;
use crate::dirent::DirentTarget;
use crate::errcode::Found;
use crate::snapshots::{snapshot, subvolume};
use crate::alloc::buckets::DiskReservation;
use crate::util::alloc::{flags::GFP_KERNEL, kvvec_insert, KVVec};
use crate::util::Printbuf;
use crate::{bch_err, bch_err_msg, bch_info, fsck_err_on, inode_fsck_err};
use core::mem::size_of;
use core::ops::ControlFlow;

/// The snapshot IDs of the keys seen so far at one position, for deciding
/// visibility while walking a btree in key order with all snapshots: a key
/// in an ancestor snapshot is hidden from a descendant that overwrote it.
///
/// C knows the type only by name: it hands a pointer to one from Rust's
/// check_key() back to Rust's __bch2_str_hash_check_key() without looking.
pub struct SnapshotsSeen {
    pos: c::bpos,
    /// In the order seen - ascending, for a walk in key order - plus any a
    /// repair added in order.
    ids: KVVec<u32>,
}

impl SnapshotsSeen {
    pub fn new() -> Self {
        SnapshotsSeen { pos: Default::default(), ids: KVVec::new() }
    }

    /// For C that takes a struct snapshots_seen * only to pass it back to
    /// Rust.
    pub(crate) fn as_opaque(&mut self) -> *mut c::snapshots_seen {
        self as *mut Self as *mut c::snapshots_seen
    }

    /// The IDs as a C snapshot_id_list, for C that reads one: borrowed, so
    /// C mustn't grow or free it.
    pub(crate) fn ids_view(&mut self) -> c::snapshot_id_list {
        let mut l: c::snapshot_id_list = Default::default();
        l.data = self.ids.as_mut_ptr();
        l.nr   = self.ids.len();
        l.size = self.ids.len();
        l
    }

    /// Record a key at @pos, starting the list over if @pos is a new
    /// position - inode and offset, the snapshot being what's recorded: as
    /// bch2_snapshots_seen_update().
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

    /// Whether a key in snapshot @ancestor is visible in its descendant @id,
    /// not overwritten by a key this list has seen: as
    /// bch2_key_visible_in_snapshot().
    pub fn key_visible(&self, trans: &BtreeTrans<'_>, id: u32, ancestor: u32) -> bool {
        debug_assert!(id <= ancestor);

        if id == ancestor {
            return true;
        }
        if !snapshot::is_ancestor(trans, id, ancestor) {
            return false;
        }

        // @id is a descendant of @ancestor: has a key been seen that
        // overwrote @ancestor - also a descendant of @ancestor, and with @id
        // as a descendant?
        !self.ids.iter().any(|&i| i != ancestor && snapshot::is_ancestor(trans, id, i))
    }

    /// Whether a reference from a key in @src points at something visible in
    /// some snapshot as a key in @dst: as bch2_ref_visible(). Assumes the @src
    /// keys are being visited in key order, with this list recording them.
    pub fn ref_visible(&self, trans: &BtreeTrans<'_>, src: u32, dst: u32) -> bool {
        if dst <= src {
            self.key_visible(trans, dst, src)
        } else {
            snapshot::is_ancestor(trans, src, dst)
        }
    }

    /// The snapshots that have overwritten the key at @pos in @btree, as if
    /// a walk had seen them there: for checking a key out of walk order, as
    /// with no SnapshotsSeen at hand, C's bch2_get_snapshot_overwrites().
    pub fn overwrites(trans: &BtreeTrans<'_>, btree: c::btree_id, pos: c::bpos)
        -> Result<Self, BchError>
    {
        let mut s = SnapshotsSeen { pos, ids: KVVec::new() };

        if !snapshot::has_children(trans.fs(), pos.snapshot) {
            return Ok(s);
        }

        // From the version before @pos's - in the snapshot with the next lower
        // ID; @pos.snapshot has children, so it isn't 0 - down
        let mut iter = BtreeIter::new(trans, btree, spos(pos.inode, pos.offset, pos.snapshot - 1),
                                      BtreeIterFlags::ALL_SNAPSHOTS);
        let ids = &mut s.ids;
        iter.for_each_reverse_norestart(spos(pos.inode, pos.offset, 0), |_, k| {
            let id = k.k.p.snapshot;
            if snapshot::is_ancestor(trans, id, pos.snapshot) &&
               !ids.iter().any(|&i| snapshot::is_ancestor(trans, id, i)) {
                ids.push(id, GFP_KERNEL)?;
            }
            Ok(())
        })?;
        Ok(s)
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

    /// Record @id as seen, keeping the list sorted: for a key a repair
    /// created at this position.
    pub fn add_inorder(&mut self, id: u32) -> Result<(), BchError> {
        match self.ids.iter().position(|&i| i >= id) {
            Some(idx) if self.ids[idx] == id => {}
            Some(idx) => kvvec_insert(&mut self.ids, idx, id)?,
            None => self.ids.push(id, GFP_KERNEL)?,
        }
        Ok(())
    }
}

/// Whether a key in @src (seen as @src_seen) and one in @dst (seen as
/// @dst_seen) are both visible in some snapshot - one being an ancestor of
/// the other, and not overwritten in between: as bch2_ref_visible2().
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

/// Every snapshot version of the inode the keys being walked belong to, for
/// the passes that walk a btree keyed by inode number (xattrs, dirents,
/// extents) in key order with all snapshots.
///
/// The versions are cached between keys of the same inode, and refetched on
/// the next inode or after a commit. walk() keeps the entries sorted by
/// snapshot ID, and each carries a per-pass count (i_sectors, subdirectories).
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

    /// The live inode version @k belongs to, as bch2_walk_inode() and then
    /// check_key_has_inode(): None if @k is a tombstone, or its inode is
    /// missing or a whiteout. A missing inode or one of the wrong type is
    /// reported, and repaired.
    ///
    /// Either may commit and return a restart. On any error the cached
    /// versions are invalidated, so the retry starts from the btree.
    pub fn walk<'a, 't, 'w>(
        &'w mut self,
        t:    TransAttempt<'a, 't>,
        iter: &mut BtreeIter<'t>,
        k:    BkeySC<'_>,
    ) -> TransResult<'a, 't, Option<Walked<'w>>> {
        let (t, i) = match self.walk_and_check(t, iter, k) {
            Ok(v) => v,
            Err(e) => {
                // Keep last_pos on this inode: a retry that saw a new inode
                // would have the caller check this one's sums half counted
                self.last_pos = k.k.p;
                self.invalidate();
                return Err(e);
            }
        };

        let Some(i) = i.filter(|&i| !self.inodes[i].whiteout) else { return t.done(None) };

        let first_this_inode = core::mem::take(&mut self.first_this_inode);
        t.done(Some(Walked { inode: &mut self.inodes[i].inode, first_this_inode }))
    }

    /// The index of the version @k resolves to, if any.
    fn walk_and_check<'a, 't>(
        &mut self,
        t:    TransAttempt<'a, 't>,
        iter: &mut BtreeIter<'t>,
        k:    BkeySC<'_>,
    ) -> TransResult<'a, 't, Option<usize>> {
        let trans = t.trans();

        if self.last_pos.inode != k.k.p.inode {
            self.get_inodes_all_snapshots(trans, k.k.p.inode)?;
        } else if self.commit_count != trans.commit_count() {
            // A commit may have updated inodes we have cached: revalidate.
            // We're mid way through walking this inode's keys, so per-inode
            // accumulations (i_sectors, subdir counts) are now partial -
            // recount instead of complaining:
            self.get_inodes_all_snapshots(trans, k.k.p.inode)?;
            self.recalculate_sums = true;
        }

        self.last_pos = k.k.p;

        let (t, i) = self.lookup_inode_for_snapshot(t, k)?;
        let t = self.check_key_has_inode(t, iter, i, k)?;
        t.done(i)
    }

    /// Load every version of inode @inum, whiteouts included, in snapshot ID
    /// order.
    fn get_inodes_all_snapshots(&mut self, trans: &BtreeTrans<'_>, inum: u64) -> Result<(), BchError> {
        let fs = trans.fs();

        // We no longer have inodes for last_pos; clear this to avoid screwing
        // up check_i_sectors/check_subdir_count if we take a transaction
        // restart here:
        self.have_inodes = false;
        self.recalculate_sums = false;
        self.inodes.clear();

        let inodes = &mut self.inodes;
        let mut iter = BtreeIter::new(trans, c::btree_id::inodes, pos(0, inum),
                                      BtreeIterFlags::ALL_SNAPSHOTS);
        iter.for_each_max_norestart(spos(0, inum, u32::MAX), |_, k| {
            inodes.push(WalkerEntry::new(fs, k), GFP_KERNEL)?;
            Ok(())
        })?;

        self.first_this_inode = true;
        self.have_inodes = true;
        self.commit_count = trans.commit_count();
        Ok(())
    }

    /// The version @k belongs to: the first - in snapshot ID order, the
    /// nearest - in its snapshot or an ancestor.
    ///
    /// That should be in @k's own snapshot, since a key's inode is always
    /// updated when the key is; if it's an ancestor's, the inode is copied
    /// (or the whiteout repeated) into @k's snapshot, committed, and a restart
    /// returned.
    fn lookup_inode_for_snapshot<'a, 't>(&mut self, t: TransAttempt<'a, 't>, k: BkeySC<'_>)
        -> TransResult<'a, 't, Option<usize>>
    {
        let trans = t.trans();
        let fs = trans.fs();
        let snapshot = k.k.p.snapshot;
        let k_snapshot = snapshot::redundant_interior(fs, snapshot).unwrap_or(snapshot);

        let Some(i) = self.inodes.iter()
            .position(|i| snapshot::is_ancestor(trans, k_snapshot, i.inode.bi_snapshot)) else {
            return t.done(None);
        };

        let e = self.inodes[i];
        let inode_snapshot = snapshot::redundant_interior(fs, e.inode.bi_snapshot)
            .unwrap_or(e.inode.bi_snapshot);

        if !fsck_err_on!(trans, k_snapshot != inode_snapshot, id::snapshot_key_missing_inode_snapshot,
                         "have key for inode {}:{snapshot} but have inode in ancestor snapshot {}\n\
                          unexpected because we should always update the inode when we update a key in that inode\n\
                          {}",
                         { self.last_pos.inode }, e.inode.bi_snapshot, k.to_text(fs))? {
            return t.done(Some(i));
        }

        let t = if !e.whiteout {
            let mut new = e.inode;
            new.bi_snapshot = snapshot;
            inode::fsck_write(t, &mut new)?
        } else {
            let whiteout = t.bkey_alloc_init(0, c::bch_bkey_type::KEY_TYPE_whiteout.0 as u8,
                                             spos(0, e.inode.bi_inum, snapshot))?;
            t.insert_with(c::btree_id::inodes, whiteout, BtreeIterFlags::CACHED,
                          UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?
        };
        let t = t.commit(None, CommitFlags::empty())?;

        // walk() invalidates on the restart: the retry refetches the versions,
        // the one just written included
        Err(t.restart(bch_errcode::BCH_ERR_transaction_restart_nested))
    }

    /// @k, in @iter's btree, has to belong to an inode of the matching type:
    /// version @i, the one it resolved to. A missing inode is recreated, or
    /// copied down from a good version in an ancestor snapshot, or - if it
    /// looks deleted - @k is; one of the wrong type gets the type its keys
    /// say it has.
    fn check_key_has_inode<'a, 't>(
        &mut self,
        t:    TransAttempt<'a, 't>,
        iter: &mut BtreeIter<'t>,
        i:    Option<usize>,
        k:    BkeySC<'_>,
    ) -> TransRet<'a, 't> {
        let trans = t.trans();
        let fs = trans.fs();
        let btree = iter.btree();

        // whiteouts and hash whiteouts are tombstones - they need no inode:
        if bkey_extent_whiteout(k.k) || k.key_type() == c::bch_bkey_type::KEY_TYPE_hash_whiteout {
            return Ok(t);
        }

        let inodes = self.inodes();
        // The version @k resolved to, unless that's a whiteout:
        let live = i.filter(|&i| !inodes[i].whiteout);

        if live.is_some_and(|l| btree_matches_i_mode(btree, inodes[l].inode.bi_mode)) {
            return Ok(t);
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
            write!(buf, ", but found good inode in older snapshot{}\n", inodes[g].inode);
        }

        write!(buf, "\nfound keys:\n");

        let inode_pos = spos(k.k.p.inode, 0, k.k.p.snapshot);
        let nr_keys = count_inode_keys(trans, inode_pos, btree, Some(&mut buf))?;
        if nr_keys == 0 {
            bch_err!(fs, "check_key_has_inode: error finding live keys in inode");
            return Err(fs.err(bch_errcode::BCH_ERR_shutdown_with_errors_unfixed).into());
        }

        if nr_keys >= COUNT_INODE_KEYS_MAX {
            write!(buf, "found {nr_keys}+ keys for this inode\n");
        } else {
            write!(buf, "found {nr_keys} keys for this inode\n");
        }

        let lost_data = fs.btree_lost_data(c::btree_id::inodes);
        if lost_data {
            write!(buf, "data was lost in inodes btree\n");
        }

        // Both write repairs edit a version in place: which one
        let fix = match live {
            None => {
                let inode_looks_deleted = good_ancestor.is_some() && nr_keys < 3 && !lost_data;
                if inode_looks_deleted {
                    write!(buf, "inode was deleted, will delete key\n");
                }

                if !inode_fsck_err!(trans, k.k.p, id::key_in_missing_inode, "{buf}")? {
                    return Ok(t);
                }

                if inode_looks_deleted {
                    return delete_stray_key(t, iter);
                }

                let Some(g) = good_ancestor else {
                    let t = reconstruct_inode(t, btree, k.k.p.snapshot, k.k.p.inode)?;
                    let t = t.commit(None, CommitFlags::NO_ENOSPC)?;
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
                    Some(b) => count_inode_keys(trans, inode_pos, b, None)? != 0,
                    None    => false,
                };

                if has_own_keys {
                    write!(buf, "inode has keys of its own type: deleting this one\n");
                } else {
                    write!(buf, "inode has no keys of its own type: changing its type to match\n");
                }

                if !inode_fsck_err!(trans, k.k.p, id::key_in_wrong_inode_type, "{buf}")? {
                    return Ok(t);
                }

                if has_own_keys {
                    return delete_stray_key(t, iter);
                }

                // A file's i_size has to cover its data, or the extents are
                // past the end and go next: as a directory, it had none
                let (ty, size) = if btree == c::btree_id::dirents {
                    (c::S_IFDIR, 0)
                } else {
                    (c::S_IFREG, extents_end(trans, k.k.p.inode, k.k.p.snapshot)?)
                };

                let inode = &mut self.inodes_mut()[l].inode;
                inode.bi_mode = (inode.bi_mode & !(c::S_IFMT as c::umode_t)) | ty as c::umode_t;
                inode.bi_size = inode.bi_size.max(size);
                self.changed_inode_type = true;
                l
            }
        };

        // fsck_write(), not the self-committing version: we're inside the
        // caller's commit loop, and that one eats restarts - and when the lazy
        // commit after it has nothing to do, the advanced restart_count leaks
        // to the caller
        let t = inode::fsck_write(t, &mut self.inodes_mut()[fix].inode)?;
        t.commit_lazy(CommitFlags::NO_ENOSPC)
    }

    /// The cached versions are stale - something changed the inodes without
    /// committing, or the walk is being retried: refetch them on the next
    /// walk, and recount the per-inode sums.
    ///
    /// That is the path a commit takes, so this is how it's spelled: the
    /// walker revalidates when the transaction's commit_count moves on from
    /// the one it recorded, and the recorded count is never ahead of it.
    pub fn invalidate(&mut self) {
        self.commit_count = self.commit_count.wrapping_sub(1);
    }

    /// The versions visible to a key in @snapshot, as for_each_visible_inode:
    /// @snapshot or its ancestors, not overwritten by a version in a
    /// descendant that @s has seen.
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

    /// Load the versions of inode @inum a key at @s's position refers to, as
    /// bch2_get_visible_inodes(): for the target of a reference (a dirent's
    /// inode), not for walking. Scans down from @s's snapshot: the versions
    /// whose key is visible from it, newest snapshot first, and in deletes()
    /// the snapshots where a whiteout hides older versions.
    pub fn get_visible(
        &mut self,
        trans: &BtreeTrans<'_>,
        s:     &SnapshotsSeen,
        inum:  u64,
    ) -> Result<(), BchError> {
        let fs = trans.fs();
        let snapshot = s.pos.snapshot;

        self.inodes.clear();
        self.deletes.clear();

        let (inodes, deletes) = (&mut self.inodes, &mut self.deletes);
        let mut iter = BtreeIter::new(trans, c::btree_id::inodes, spos(0, inum, snapshot),
                                      BtreeIterFlags::ALL_SNAPSHOTS);
        iter.for_each_reverse_norestart(pos(0, inum), |_, k| {
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
        })
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

/// Delete @iter's key, a stray, and start the caller's key over: committed,
/// and a restart, because the passes' per-key checks go on with the key they
/// were handed - a deleted dirent would be rehashed back in, and have its
/// target's backpointer pointed at it.
///
/// With a disk reservation: an extents update needs one to put whatever its
/// triggers charge. A deletion charges nothing, so an empty one does.
fn delete_stray_key<'a, 't>(t: TransAttempt<'a, 't>, iter: &mut BtreeIter<'t>) -> TransRet<'a, 't> {
    let res = DiskReservation::new(t.trans().fs());
    let t = t.delete_at(iter, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
    let t = t.commit(Some(&res), CommitFlags::NO_ENOSPC)?;
    Err(t.restart(bch_errcode::BCH_ERR_transaction_restart_commit))
}

/// Where @inum's last extent, as @snapshot sees it, ends, in bytes: the
/// i_size its data needs.
fn extents_end(trans: &BtreeTrans<'_>, inum: u64, snapshot: u32) -> Result<u64, BchError> {
    let mut iter = BtreeIter::new(trans, c::btree_id::extents,
                                  spos(inum, u64::MAX, snapshot), BtreeIterFlags::empty());
    Ok(iter.peek_prev_min(pos(inum, 0))?.map_or(0, |k| k.k.p.offset << 9))
}

/// Recreate missing inode @inum:@snapshot from the keys found in @btree -
/// a directory for dirents, a file sized to its extents - for the caller to
/// commit.
pub fn reconstruct_inode<'a, 't>(
    t:        TransAttempt<'a, 't>,
    btree:    c::btree_id,
    snapshot: u32,
    inum:     u64,
) -> TransRet<'a, 't> {
    let trans = t.trans();
    let fs = trans.fs();

    let (mode, size) = match btree {
        // may race with repair deleting the extents that triggered us
        c::btree_id::extents => (c::S_IFREG, extents_end(trans, inum, snapshot)?),
        c::btree_id::dirents => (c::S_IFDIR, 0),
        c::btree_id::xattrs  => (c::S_IFREG, 0),
        _ => unreachable!("reconstruct_inode() for btree {}", btree as u32),
    };

    let mut new = inode::init(fs, 0, 0, (mode | 0o600) as c::umode_t, 0, None);
    new.bi_size     = size;
    new.bi_inum     = inum;
    new.bi_snapshot = snapshot;

    // Recover the hash info if any version of this inode survives anywhere.
    //
    // bi_hash_seed and the str_hash type are the same in every snapshot
    // version of an inode - repair_inode_hash_info() exists to enforce that -
    // so a descendant will do when no ancestor is left. Btree node loss takes
    // out one snapshot's inode key while leaving another's, and an
    // ancestor-only search calls that unrecoverable and falls back to the
    // random seed init() left in @new. That puts every dirent already under
    // this directory at the wrong hash offset: lookups miss, so creates insert
    // duplicates instead of overwriting, and the directory quietly becomes
    // untraversable.
    let hash_src = match inode::find_oldest_snapshot(trans, inum, snapshot) {
        Err(e) if e.matches(c::ENOENT) => inode::find_any_snapshot(trans, inum),
        r => r,
    }.found()?;
    if let Some(src) = hash_src {
        new.bi_hash_seed = src.bi_hash_seed;
        new.set_str_hash(src.str_hash());
    }

    inode::fsck_write(t, &mut new)
}

/// The root inode for recreating missing subvolume @subvol at @snapshot:
/// @inum if the caller knows it, else found. fsck_repair_unimplemented,
/// logged, if it can't be done - for reconstruct_subvol(), and decided before
/// anything is queued, so a caller can fall back to reporting.
///
/// @snapshot has to be a leaf: the key reconstruct_subvol() writes sets the
/// snapshot's subvol backref, and bch2_snapshot_validate() rejects a subvol
/// on a node with children.
pub fn reconstruct_subvol_root(
    trans:    &BtreeTrans<'_>,
    snapshot: u32,
    subvol:   u32,
    inum:     Option<u64>,
) -> Result<u64, BchError> {
    let fs = trans.fs();

    if !snapshot::is_leaf(fs, snapshot)? {
        bch_err!(fs, "need to reconstruct subvol {subvol}, but snapshot {snapshot} is an interior node");
        return Err(fs.err(bch_errcode::BCH_ERR_fsck_repair_unimplemented));
    }

    if let Some(inum) = inum {
        return Ok(inum);
    }

    // Find the root inode rather than minting one: the inode carrying
    // bi_subvol == @subvol is the root, and when it's the subvolume key that
    // went missing that inode is still there. Creating a second one would
    // leave two claimants for the same subvolume and the real contents
    // orphaned behind the new empty root.
    //
    // It can't be deferred to a later pass either - bch2_subvolume_validate()
    // rejects a subvolume key with inode == 0 (subvol_inode_bad), so the key
    // can't be written at all until we know it.
    let mut root = None;
    let mut iter = BtreeIter::new(trans, c::btree_id::inodes, pos(0, 0),
                                  BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);
    iter.for_each_norestart(|_, k| {
        if inode::bkey_is_inode(k.k) {
            let u = inode::unpack(fs, k);
            if u.bi_subvol == subvol {
                root = Some(u.bi_inum);
                return Ok(ControlFlow::Break(()));
            }
        }
        Ok(ControlFlow::Continue(()))
    })?;

    root.ok_or_else(|| {
        bch_err!(fs, "no root inode found for subvol {subvol}, can't reconstruct");
        fs.err(bch_errcode::BCH_ERR_fsck_repair_unimplemented)
    })
}

/// Recreate missing subvolume key @subvol, at leaf @snapshot with root inode
/// @root - from reconstruct_subvol_root() - and point the snapshot, and its
/// tree if it has no master subvolume, back at it.
pub fn reconstruct_subvol<'a, 't>(
    t:        TransAttempt<'a, 't>,
    snapshot: u32,
    subvol:   u32,
    root:     u64,
) -> TransRet<'a, 't> {
    let fs = t.trans().fs();

    bch_info!(fs, "reconstructing subvol {subvol} with root inode {root}");

    let mut k = t.bkey_alloc_typed::<c::bkey_i_subvolume>()?;
    k.k_mut().p = pos(0, subvol as u64);
    let v = k.k_i_mut().as_mut_subvolume().expect("a subvolume key");
    v.snapshot = snapshot.to_le();
    v.inode    = root.to_le();
    v.set_state(c::bch_subvolume_state::SUBVOLUME_STATE_live);
    let t = t.insert(c::btree_id::subvolumes, k, UpdateTriggerFlags::empty())?;

    let mut s = bch_err_msg!(fs,
        t.bkey_get_mut(c::btree_id::snapshots, pos(0, snapshot as u64), UpdateTriggerFlags::empty(),
                       c::bch_bkey_type::KEY_TYPE_snapshot, size_of::<c::bkey_i_snapshot>()),
        "getting snapshot {snapshot}")?;
    let s = s.k_i_mut().as_mut_snapshot().expect("a snapshot key");
    let tree = s.tree();
    s.subvol = subvol.to_le();
    s.set_state(c::bch_snapshot_state::SNAPSHOT_STATE_live);

    let mut st = bch_err_msg!(fs,
        t.bkey_get_mut(c::btree_id::snapshot_trees, pos(0, tree as u64), UpdateTriggerFlags::empty(),
                       c::bch_bkey_type::KEY_TYPE_snapshot_tree, size_of::<c::bkey_i_snapshot_tree>()),
        "getting snapshot tree {tree}")?;
    let st = st.k_i_mut().as_mut_snapshot_tree().expect("a snapshot tree key");
    if st.master_subvol == 0 {
        st.master_subvol = subvol.to_le();
    }

    Ok(t)
}

/// Whether an inode of mode @mode can own keys in @btree.
fn btree_matches_i_mode(btree: c::btree_id, mode: c::umode_t) -> bool {
    let fmt = mode as u32 & c::S_IFMT;
    match btree {
        c::btree_id::extents => fmt == c::S_IFREG || fmt == c::S_IFLNK,
        c::btree_id::dirents => fmt == c::S_IFDIR,
        c::btree_id::xattrs  => true,
        _ => unreachable!("check_key_has_inode() on btree {}", btree as u32),
    }
}

/// Where count_inode_keys() stops counting.
const COUNT_INODE_KEYS_MAX: u32 = 100;

/// How many keys inode @inode_pos has in @btree, as its version in
/// @inode_pos's snapshot sees them - up to COUNT_INODE_KEYS_MAX - printing
/// the first ten to @out.
fn count_inode_keys(
    trans:     &BtreeTrans<'_>,
    inode_pos: c::bpos,
    btree:     c::btree_id,
    mut out:   Option<&mut Printbuf>,
) -> Result<u32, BchError> {
    let fs = trans.fs();
    let mut nr_keys = 0;
    let mut iter = BtreeIter::new(trans, btree, inode_pos, BtreeIterFlags::empty());

    iter.for_each_max_norestart(pos(inode_pos.inode, u64::MAX), |_, k| {
        // Error keys count: they're placeholders for unreadable data,
        // evidence the inode had contents. Hash whiteouts are just
        // tombstones:
        if k.key_type() == c::bch_bkey_type::KEY_TYPE_hash_whiteout {
            return Ok(ControlFlow::Continue(()));
        }

        nr_keys += 1;
        if let Some(out) = out.as_deref_mut().filter(|_| nr_keys <= 10) {
            write!(out, "{}\n", k.to_text(fs));
        }

        Ok(if nr_keys >= COUNT_INODE_KEYS_MAX { ControlFlow::Break(()) } else { ControlFlow::Continue(()) })
    })?;

    Ok(nr_keys)
}

/// @new, a key a hash table repair just wrote, may be a dirent at a new
/// position: point what it names back at it - a subvolume's root inode, or
/// the versions of an inode visible from it, by @s.
pub fn fsck_update_backpointers<'a, 't>(
    t:   TransAttempt<'a, 't>,
    s:   &mut SnapshotsSeen,
    new: &c::bkey_i,
) -> TransRet<'a, 't> {
    let trans = t.trans();
    let Some(d) = BkeySC::from(new).as_dirent() else { return Ok(t) };
    let (dir, offset) = (new.k.p.inode, new.k.p.offset);
    let points_here = |i: &c::bch_inode_unpacked| i.bi_dir == dir && i.bi_dir_offset == offset;

    match d.target() {
        // A subvolume dirent's backpointer lives on the child subvolume's root
        // inode, same as a regular inode - see bch2_inode_get_dirent(). A
        // dangling subvol dirent (subvol or root inode gone) is
        // check_subvols/check_dirents' problem, not ours.
        DirentTarget::Subvol { child, .. } => {
            let Some(subvol) = subvolume::get(trans, child, false).found()? else { return Ok(t) };
            let Some(mut root) = inode::find_by_inum_snapshot(trans, u64::from_le(subvol.inode),
                                                              u32::from_le(subvol.snapshot),
                                                              BtreeIterFlags::empty()).found()? else {
                return Ok(t);
            };
            if points_here(&root) {
                return Ok(t);
            }

            root.bi_dir        = dir;
            root.bi_dir_offset = offset;
            inode::fsck_write(t, &mut root)
        }
        DirentTarget::Inode(inum) => {
            let mut target = InodeWalker::new();
            target.get_visible(trans, s, inum)?;

            // A backpointer is the (bi_dir, bi_dir_offset) pair - compare and
            // set both, or an offset match into the wrong directory skips a
            // broken backpointer, and an offset-only write manufactures one.
            //
            // Skip before the write: fsck_write() allocates a bkey_inode_buf
            // of trans mem per call, and this runs once per visible version
            // in one transaction - an already-correct backpointer must cost
            // nothing, both to bound trans mem and so a re-run over
            // partially-repaired state shrinks instead of repeating the batch.
            let mut t = t;
            for i in target.inodes_mut().iter_mut().filter(|i| !points_here(&i.inode)) {
                i.inode.bi_dir        = dir;
                i.inode.bi_dir_offset = offset;
                t = inode::fsck_write(t, &mut i.inode)?;
            }
            Ok(t)
        }
    }
}

impl Default for SnapshotsSeen {
    fn default() -> Self {
        Self::new()
    }
}
