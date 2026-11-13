// SPDX-License-Identifier: GPL-2.0

//! The inode cache: every VFS inode of ours, by subvolume and inode number -
//! bcachefs's own, never the VFS's inode hash (inode_fake_hash()). Only fully
//! made inodes go in, after their commit, so there's no I_NEW protocol.
//!
//! And which subvolumes hold an inode number - a cached inode, or a tmpfile's
//! guard before it has one: fsck asks, before it deletes an unlinked inode,
//! whether a version of it in a descendant snapshot is open.
//!
//! An inode's in if it's in the table: a lookup holds RCU from the table to
//! the inode's i_lock, so it needs nothing else to know. One that finds an
//! inode the VFS is freeing - evict's to remove, which the VFS calls with
//! I_FREEING set - waits for it to go.

use crate::btree::iter::TransAttempt;
use crate::c;
use crate::data_structures::cuckoo::{Cuckoo, CuckooMultimap, Entry};
use crate::errcode::{Errcode, BchError};
use crate::format::Bpos;
use crate::fs::Fs;
use crate::locking::MemallocFlags;
use crate::snapshots::{snapshot, subvolume};
use crate::util::alloc::KVVec;
use crate::os::kernel::HZ;
use crate::util::rcu::read_lock;
use core::ptr::NonNull;

use super::inode::{InodeInfo, InodeRef};
use super::new_inode::NewInode;
use super::vfs_inode::VfsInode;

/// An inode as the cache keys it: by its subvol_inum, set before it's in and
/// unchanged while it is.
#[repr(transparent)]
pub struct Cached(InodeInfo);

impl Cached {
    fn of(inode: &InodeInfo) -> NonNull<Cached> {
        NonNull::from(inode).cast()
    }
}

crate::keyed_by!(Cached, |e| &e.0.ei_inum => c::subvol_inum);

#[derive(Default)]
pub struct InodeCache {
    inodes:  Cuckoo<Entry<Cached>>,
    /// Inode number -> subvolumes holding it
    by_inum: CuckooMultimap<u64, u32, 2>,
}

impl InodeCache {
    /// @inum's, if it's in: referenced. One being freed is waited for - up to
    /// ten seconds, then looked up again - unlocking @t, if there's one: a
    /// restart, then.
    pub fn find(&self, t: Option<&TransAttempt<'_, '_>>, inum: c::subvol_inum)
        -> Result<Option<InodeRef>, BchError>
    {
        loop {
            let freeing = {
                let g = read_lock();
                let Some(cached) = self.inodes.get_entry(&inum, &g) else { return Ok(None) };
                let inode = &cached.0;
                if let Some(r) = inode.iget(&inode.vfs().i_lock().lock()) {
                    return Ok(Some(r));
                }
                NonNull::from(inode.vfs())
            };

            // remove()'s wake, which comes after it takes the inode out: a
            // wait that finds it out, once on the waitqueue, won't miss it
            let wait = || {
                VfsInode::wait_new_bit(freeing, || {
                    let g = read_lock();
                    self.inodes.get_entry(&inum, &g).is_some_and(|c| NonNull::from(c.0.vfs()) == freeing)
                }, (HZ * 10) as _);
                Ok(())
            };
            match t {
                Some(t) => t.drop_locks_do(wait)?,
                None => wait()?,
            }
        }
    }

    /// @inode in - or the one a racing lookup put in first, @inode given up.
    /// Fails, @inode given up, only if there's no memory.
    pub fn insert(&self, t: &TransAttempt<'_, '_>, inode: NewInode) -> Result<InodeRef, BchError> {
        self.insert_mode(Some(t), inode, false)
    }

    /// insert(), for a new inode, whose create has committed: there's no other
    /// to race with, and no failing - __GFP_NOFAIL, if it has to allocate.
    pub fn insert_nofail(&self, inode: NewInode) -> InodeRef {
        self.insert_mode(None, inode, true).expect("a nofail insert of a new inode failed")
    }

    /// Room made ahead, by a caller that can block: Cuckoo::make_room().
    /// Failing, insert() is left to make it.
    pub fn make_room(&self) {
        let _noio = MemallocFlags::noio();
        let _ = self.inodes.make_room();
        let _ = self.by_inum.make_room();
    }

    fn insert_mode(&self, t: Option<&TransAttempt<'_, '_>>, inode: NewInode, nofail: bool)
        -> Result<InodeRef, BchError>
    {
        // under the caller's btree locks, often: reclaim mustn't recurse into them
        let _noio = MemallocFlags::noio();
        let inum = inode.ei_inum;

        if nofail {
            let (i, subvol) = holder(inum);
            self.by_inum.add_nofail(i, subvol);
        } else {
            self.add_holder(inode.fs(), inum)?;
        }

        let lost = loop {
            // SAFETY: freed a grace period after it's out, as the VFS frees
            // inodes, by RCU; its key set
            let e = unsafe { Entry::new(Cached::of(&inode)) };
            let inserted = if nofail { Ok(self.inodes.insert_new_nofail(e)) } else { self.inodes.insert_new(e) };
            match inserted {
                Ok(Ok(())) => break None,
                // another's in first: it, if it's still there
                Ok(Err(_)) => match self.find(t, inum).transpose() {
                    Some(old) => break Some(old),
                    None => continue,
                },
                Err(_) => break Some(Err(inode.fs().err(Errcode::BCH_ERR_ENOMEM_inode_cache_insert))),
            }
        };
        if let Some(ret) = lost {
            self.remove_holder(inum);
            return ret;
        }

        let inode = inode.into_ref();
        inode.vfs().fake_hash();
        inode.vfs().sb_list_add();
        inode.fs().vfs.inodes.set(inode.ei_inodes_idx, NonNull::from(&*inode).cast());
        Ok(inode)
    }

    /// @inode out, if it's in - and lookups waiting for it to go woken.
    pub fn remove(&self, inode: &InodeInfo) {
        if !self.inodes.remove_entry(&inode.ei_inum, Cached::of(inode)) {
            return;
        }
        self.remove_holder(inode.ei_inum);
        inode.vfs().unfake_hash();
        inode.vfs().wake_new_bit();
    }

    /// @inum held, for or_descendents_is_open(), until the guard's dropped.
    pub fn hold(&self, fs: &Fs, inum: c::subvol_inum) -> Result<Held<'_>, BchError> {
        self.add_holder(fs, inum)?;
        Ok(Held { cache: self, inum })
    }

    /// @inum held by an inode in the cache - or by a Held.
    fn add_holder(&self, fs: &Fs, inum: c::subvol_inum) -> Result<(), BchError> {
        let _noio = MemallocFlags::noio();
        let (i, subvol) = holder(inum);
        self.by_inum.add(i, subvol).map_err(|_| fs.err(Errcode::BCH_ERR_ENOMEM_inode_cache_insert))
    }

    fn remove_holder(&self, inum: c::subvol_inum) {
        let (i, subvol) = holder(inum);
        let was = self.by_inum.remove(&i, &subvol);
        assert!(was, "inode cache: {}:{} not held by its subvolume", inum.subvol, inum.inum);
    }

    /// The subvolumes holding inode number @inum.
    fn holders(&self, inum: u64) -> Result<KVVec<u32>, BchError> {
        let _noio = MemallocFlags::noio();
        let mut subvols = KVVec::new();
        loop {
            // copied out under a spinlock: room made first, outside it
            let n = self.by_inum.with(&inum, |s| s.len());
            subvols.reserve(n)?;
            let fit = self.by_inum.with(&inum, |s| {
                s.len() <= subvols.capacity() && {
                    subvols.clear();
                    subvols.extend_from_slice(s).is_ok()
                }
            });
            if fit {
                return Ok(subvols);
            }
        }
    }
}

/// Marks an inode number as held for as long as the guard is in scope, without
/// an inode to hang it on.
///
/// O_TMPFILE commits an inode that is already unlinked, so from the commit
/// onwards a scanning fsck pass can see the key - while create() is still
/// several steps away from having an inode in the cache.
/// or_descendents_is_open() would answer "not open" for a file the caller is
/// about to be handed a descriptor for, and check_inode() deletes exactly
/// that.
pub struct Held<'c> {
    cache: &'c InodeCache,
    inum:  c::subvol_inum,
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.cache.remove_holder(self.inum)
    }
}

/// @inum as by_inum has it: its inode number, held by its subvolume.
fn holder(inum: c::subvol_inum) -> (u64, u32) {
    (inum.inum, inum.subvol as u32)
}

/// Whether the inode at @pos, or a version of it in a descendant snapshot, is
/// open - so fsck mustn't delete it.
pub fn or_descendents_is_open(t: &TransAttempt<'_, '_>, pos: Bpos) -> Result<bool, BchError> {
    let fs = t.fs();
    if !fs.flags.get(c::FsFlag::started) {
        return Ok(false);
    }

    for subvol in fs.vfs.inode_cache.holders(pos.offset)? {
        let snap = subvolume::get_snapshot(t, subvol)?;
        if snapshot::is_ancestor(t, snap, pos.snapshot) {
            return Ok(true);
        }
    }
    Ok(false)
}
