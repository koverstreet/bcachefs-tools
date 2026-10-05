// SPDX-License-Identifier: GPL-2.0

//! fsck helpers shared by the passes (fs/check.h).

use crate::btree::bkey::BkeySC;
use crate::btree::iter::{BtreeIter, BtreeTrans};
use crate::c;
use crate::errcode::{ret_to_result_void as ret_to_result, BchError};
use crate::fs::Fs;
use crate::str_hash::HashTable;

/// The snapshot IDs of the keys seen so far at one position, for deciding
/// visibility while walking a btree in key order with all snapshots: a key
/// in an ancestor snapshot is hidden from a descendant that overwrote it.
///
/// The ID list is a darray C allocates, so C frees it. Transparent, so C can
/// pass a struct snapshots_seen * as an Option<&mut SnapshotsSeen>.
#[repr(transparent)]
pub struct SnapshotsSeen(c::snapshots_seen);

impl SnapshotsSeen {
    pub fn new() -> Self {
        SnapshotsSeen(unsafe { c::snapshots_seen_init() })
    }

    pub(crate) fn raw_mut(&mut self) -> *mut c::snapshots_seen {
        &mut self.0
    }

    /// The IDs themselves, for C that takes a snapshot_id_list.
    pub(crate) fn ids_raw_mut(&mut self) -> *mut c::snapshot_id_list {
        &mut self.0.ids
    }

    /// Record a key at @pos, starting the list over if @pos is a new
    /// position: as bch2_snapshots_seen_update().
    pub fn update(&mut self, fs: &Fs, btree: c::btree_id, pos: c::bpos) -> Result<(), BchError> {
        ret_to_result(unsafe { c::bch2_snapshots_seen_update(fs.raw, &mut self.0, btree, pos) })
    }

    /// Whether a reference from a key in @src points at something visible in
    /// some snapshot as a key in @dst: as bch2_ref_visible(). Assumes the @src
    /// keys are being visited in key order, with this list recording them.
    pub fn ref_visible(&mut self, trans: &BtreeTrans<'_>, src: u32, dst: u32) -> bool {
        unsafe { c::bch2_ref_visible(trans.raw(), &mut self.0, src, dst) }
    }

    /// The snapshots that have overwritten the key at @pos in @btree, as if
    /// a walk had seen them there: for checking a key out of walk order, as
    /// with no SnapshotsSeen at hand, C's bch2_get_snapshot_overwrites().
    pub fn overwrites(trans: &BtreeTrans<'_>, btree: c::btree_id, pos: c::bpos)
        -> Result<Self, BchError>
    {
        let mut s = Self::new();
        s.0.pos = pos;
        ret_to_result(unsafe {
            c::bch2_get_snapshot_overwrites(trans.raw(), btree, pos, &mut s.0.ids)
        })?;
        Ok(s)
    }

    /// Whether a key in snapshot @id has been seen at this position.
    pub fn has_id(&self, id: u32) -> bool {
        unsafe { darray_slice(self.0.ids.data, self.0.ids.nr) }.contains(&id)
    }

    /// A copy, to keep the list as it was at this key while the walk goes
    /// on: as bch2_snapshots_seen_copy().
    pub fn try_clone(&self, fs: &Fs) -> Result<Self, BchError> {
        let mut new = Self::new();
        ret_to_result(unsafe {
            c::bch2_snapshots_seen_copy(fs.raw, &mut new.0, &self.0 as *const _ as *mut _)
        })?;
        Ok(new)
    }

    /// Record @id as seen, keeping the list sorted: for a key a repair
    /// created at this position: as bch2_snapshots_seen_add_inorder().
    pub fn add_inorder(&mut self, fs: &Fs, id: u32) -> Result<(), BchError> {
        ret_to_result(unsafe { c::bch2_snapshots_seen_add_inorder(fs.raw, &mut self.0, id) })
    }
}

/// Whether a key in @src (seen as @src_seen) and one in @dst (seen as
/// @dst_seen) are both visible in some snapshot - one being an ancestor of
/// the other, and not overwritten in between: as bch2_ref_visible2().
pub fn ref_visible2(
    trans:    &BtreeTrans<'_>,
    src:      u32,
    src_seen: &mut SnapshotsSeen,
    dst:      u32,
    dst_seen: &mut SnapshotsSeen,
) -> bool {
    unsafe { c::bch2_ref_visible2(trans.raw(), src, &mut src_seen.0, dst, &mut dst_seen.0) != 0 }
}

/// A C darray's live elements.
///
/// # Safety
/// @data and @nr are a darray's, and it outlives the slice unmodified.
unsafe fn darray_slice<'a, T>(data: *mut T, nr: usize) -> &'a [T] {
    if nr == 0 { &[] } else { unsafe { core::slice::from_raw_parts(data, nr) } }
}

/// As darray_slice(), mutably.
///
/// # Safety
/// As darray_slice(), and nothing else references the elements.
unsafe fn darray_slice_mut<'a, T>(data: *mut T, nr: usize) -> &'a mut [T] {
    if nr == 0 { &mut [] } else { unsafe { core::slice::from_raw_parts_mut(data, nr) } }
}

/// Every snapshot version of the inode the keys being walked belong to, for
/// the passes that walk a btree keyed by inode number (xattrs, dirents,
/// extents) in key order with all snapshots: over C's struct inode_walker,
/// whose internals - bch2_walk_inode() and the lookups under it - are still
/// C, as is bch2_fsck_update_backpointers(), which uses one too.
///
/// The versions are cached between keys of the same inode, and refetched on
/// the next inode or after a commit. walk() keeps the entries sorted by
/// snapshot ID, and each carries a per-pass count (i_sectors, subdirectories).
pub struct InodeWalker(c::inode_walker);

/// The live inode version a key belongs to, from InodeWalker::walk().
pub struct Walked<'w> {
    pub inode:            &'w mut c::bch_inode_unpacked,
    /// The first key walked in this inode: per-inode state such as the
    /// bch_hash_info is set up here. Reading it clears it.
    pub first_this_inode: bool,
}

impl InodeWalker {
    pub fn new() -> Self {
        InodeWalker(Default::default())
    }

    /// The live inode version @k belongs to, as bch2_walk_inode() and then
    /// bch2_check_key_has_inode(): None if @k is a tombstone, or its inode is
    /// missing or a whiteout. A missing inode is reported, and repaired by
    /// deleting @k or recreating the inode.
    ///
    /// Either may commit and return a restart. On any error the cached
    /// versions are invalidated, so the retry starts from the btree.
    pub fn walk(
        &mut self,
        trans: &BtreeTrans<'_>,
        iter:  &mut BtreeIter<'_>,
        k:     BkeySC<'_>,
    ) -> Result<Option<Walked<'_>>, BchError> {
        let i = unsafe { c::bch2_walk_inode(trans.raw(), &mut self.0, k.to_raw()) };
        // bch2_check_key_has_inode() returns an ERR_PTR() from the walk as its error.
        let ret = unsafe {
            c::bch2_check_key_has_inode(trans.raw(), iter.raw_mut(), &mut self.0, i, k.to_raw())
        };
        if let Err(e) = ret_to_result(ret) {
            // Restarts from the repairs move last_pos off this inode (C's
            // last_pos.inode--), which would make the next walk see a new
            // inode and have the caller check this one's sums half counted:
            self.0.last_pos = k.k.p;
            self.invalidate();
            return Err(e);
        }

        let Some(i) = (unsafe { i.as_mut() }) else { return Ok(None) };
        if i.whiteout {
            return Ok(None);
        }

        let first_this_inode = core::mem::take(&mut self.0.first_this_inode);
        Ok(Some(Walked { inode: &mut i.inode, first_this_inode }))
    }

    /// The cached versions are stale - something changed the inodes without
    /// committing, or the walk is being retried: refetch them on the next
    /// walk, and recount the per-inode sums.
    ///
    /// That is the path a commit takes, so this is how it's spelled: the
    /// walker revalidates when the transaction's commit_count moves on from
    /// the one it recorded, and the recorded count is never ahead of it.
    pub fn invalidate(&mut self) {
        self.0.commit_count = self.0.commit_count.wrapping_sub(1);
    }

    /// The versions visible to a key in @snapshot, as for_each_visible_inode:
    /// @snapshot or its ancestors, not overwritten by a version in a
    /// descendant that @s has seen.
    pub fn visible_mut<'a>(
        &'a mut self,
        trans:    &'a BtreeTrans<'_>,
        s:        &'a mut SnapshotsSeen,
        snapshot: u32,
    ) -> impl Iterator<Item = &'a mut c::inode_walker_entry> {
        self.inodes_mut().iter_mut()
            .take_while(move |i| i.inode.bi_snapshot <= snapshot)
            .filter(move |i| unsafe {
                c::bch2_key_visible_in_snapshot(trans.raw(), &mut s.0, i.inode.bi_snapshot, snapshot)
            })
    }

    /// Load the versions of inode @inum a key at @s's position refers to, as
    /// bch2_get_visible_inodes(): for the target of a reference (a dirent's
    /// inode), not for walking. Scans down from @s's snapshot: the versions
    /// whose key is visible from it, newest snapshot first, and in deletes()
    /// the snapshots where a whiteout hides older versions.
    pub fn get_visible(
        &mut self,
        trans: &BtreeTrans<'_>,
        s:     &mut SnapshotsSeen,
        inum:  u64,
    ) -> Result<(), BchError> {
        ret_to_result(unsafe { c::bch2_get_visible_inodes(trans.raw(), &mut self.0, &mut s.0, inum) })
    }

    pub fn inodes(&self) -> &[c::inode_walker_entry] {
        unsafe { darray_slice(self.0.inodes.data, self.0.inodes.nr) }
    }

    pub fn inodes_mut(&mut self) -> &mut [c::inode_walker_entry] {
        unsafe { darray_slice_mut(self.0.inodes.data, self.0.inodes.nr) }
    }

    /// From get_visible(): snapshots where the inode was deleted.
    pub fn deletes(&self) -> &[u32] {
        unsafe { darray_slice(self.0.deletes.data, self.0.deletes.nr) }
    }

    /// The inode whose versions walk() has loaded, if any.
    pub fn cur_inum(&self) -> Option<u64> {
        self.0.have_inodes.then_some(self.0.last_pos.inode)
    }

    /// The versions were refetched partway through the inode, so its per-pass
    /// counts are partial: recount them rather than reporting a mismatch.
    pub fn recalculate_sums(&self) -> bool {
        self.0.recalculate_sums
    }

    /// A repair changed what this inode's keys add up to: recount at the end
    /// of the inode instead of reporting the mismatch.
    pub fn set_recalculate_sums(&mut self) {
        self.0.recalculate_sums = true;
    }
}

impl Default for InodeWalker {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for InodeWalker {
    fn drop(&mut self) {
        unsafe { c::inode_walker_exit(&mut self.0) }
    }
}

/// Recreate missing inode @inum:@snapshot from the keys found in @btree -
/// a directory for dirents, a file sized to its extents - for the caller to
/// commit: as bch2_reconstruct_inode().
pub fn reconstruct_inode(
    trans:    &BtreeTrans<'_>,
    btree:    c::btree_id,
    snapshot: u32,
    inum:     u64,
) -> Result<(), BchError> {
    ret_to_result(unsafe { c::bch2_reconstruct_inode(trans.raw(), btree, snapshot, inum) })
}

/// Recreate the missing subvolume key @subvol, at leaf @snapshot - pass 0
/// for @inum to have it found from the inode carrying bi_subvol: as
/// bch2_reconstruct_subvol().
pub fn reconstruct_subvol(
    trans:    &BtreeTrans<'_>,
    snapshot: u32,
    subvol:   u32,
    inum:     u64,
) -> Result<(), BchError> {
    ret_to_result(unsafe { c::bch2_reconstruct_subvol(trans.raw(), snapshot, subvol, inum) })
}

/// @new, a key just written to hash table @T, may have moved a dirent: point
/// the backpointers of the inodes it names, in the snapshots that see it, at
/// its new position: as bch2_fsck_update_backpointers().
pub fn fsck_update_backpointers<T: HashTable>(
    trans:     &BtreeTrans<'_>,
    s:         &mut SnapshotsSeen,
    hash_info: &mut c::bch_hash_info,
    new:       &mut c::bkey_i,
) -> Result<(), BchError> {
    ret_to_result(unsafe {
        c::bch2_fsck_update_backpointers(trans.raw(), &mut s.0, *T::desc(), hash_info, new)
    })
}

/// Link @inode into lost+found: as bch2_reattach_inode().
pub fn reattach_inode(
    trans: &BtreeTrans<'_>,
    inode: &mut c::bch_inode_unpacked,
) -> Result<(), BchError> {
    ret_to_result(unsafe { c::bch2_reattach_inode(trans.raw(), inode) })
}

/// Whether @inode is unreachable and has to be reattached: no backpointer,
/// not unlinked, and not the root or an old version of a subvolume root: as
/// bch2_inode_should_reattach().
pub fn inode_should_reattach(inode: &c::bch_inode_unpacked) -> bool {
    unsafe { c::bch2_inode_should_reattach(inode as *const _ as *mut _) }
}

impl Default for SnapshotsSeen {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SnapshotsSeen {
    fn drop(&mut self) {
        unsafe { c::snapshots_seen_exit(&mut self.0) }
    }
}
