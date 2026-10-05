// SPDX-License-Identifier: GPL-2.0

//! fsck: the check_root recovery pass - the root subvolume and root directory
//! exist, and the root is a directory; recreate them if not.
//!
//! A missing root subvolume key isn't the only record of root's snapshot:
//! snapshot nodes carry a subvol backref, so its active snapshot is the leaf
//! claiming BCACHEFS_ROOT_SUBVOL. Only when no node does - a fresh
//! filesystem, or the snapshots btree is gone too - is U32_MAX, the initial
//! snapshot id, right: on a snapshotted filesystem U32_MAX is an interior
//! node, and a subvolume pointing at it is subvol_snapshot_not_leaf, which
//! has no repair.

use crate::btree::bkey::{pos, POS_MIN};
use crate::btree::iter::{
    commit_do, BtreeIter, BtreeIterFlags, CommitFlags, TransAttempt, TransRet,
    UpdateTriggerFlags,
};
use crate::c;
use crate::errcode::{BchError, Found};
use crate::fs::Fs;
use crate::init::error::id;
use crate::inode;
use crate::snapshots::subvolume;
use crate::{bch_err_msg, mustfix_fsck_err, mustfix_fsck_err_on};
use core::mem::size_of;
use core::ops::ControlFlow;

/// The leaf snapshot claiming the root subvolume, if any: 0 if none does.
fn root_snapshot_from_snapshots(t: &TransAttempt<'_, '_>) -> Result<u32, BchError> {
    let mut root_snapshot = 0;
    let mut iter = BtreeIter::new(t.trans(), c::btree_id::snapshots, POS_MIN,
                                  BtreeIterFlags::empty());

    iter.for_each_norestart(|_, k| {
        if let Some(s) = k.as_snapshot() {
            if u32::from_le(s.subvol) == c::BCACHEFS_ROOT_SUBVOL && s.children[0] == 0 {
                root_snapshot = k.k.p.offset as u32;
                return Ok(ControlFlow::Break(()));
            }
        }
        Ok(ControlFlow::Continue(()))
    })?;

    Ok(root_snapshot)
}

fn check_root_trans<'a, 't>(t: TransAttempt<'a, 't>) -> TransRet<'a, 't> {
    let trans = t.trans();
    let fs = trans.fs();

    let (snapshot, inum, t) = match subvolume::get(trans, c::BCACHEFS_ROOT_SUBVOL, false).found()? {
        Some(s) => (u32::from_le(s.snapshot), u64::from_le(s.inode), t),
        None => {
            // Inside the caller's commit_do(): restarts propagate out to it.
            let root_snapshot = root_snapshot_from_snapshots(&t)?;

            // mustfix: true, or an error - never left unfixed.
            if !mustfix_fsck_err!(trans, id::root_subvol_missing, "root subvol missing")? {
                return Ok(t);
            }

            let snapshot = if root_snapshot != 0 { root_snapshot } else { u32::MAX };
            let inum = c::BCACHEFS_ROOT_INO as u64;

            let mut k = t.bkey_alloc_init(size_of::<c::bch_subvolume>() / size_of::<u64>(),
                                          c::bch_bkey_type::KEY_TYPE_subvolume.0 as u8,
                                          pos(0, c::BCACHEFS_ROOT_SUBVOL as u64))?;
            let v = k.k_i_mut().as_mut_subvolume().expect("allocated as a subvolume");
            v.flags    = 0;
            v.snapshot = snapshot.to_le();
            v.inode    = inum.to_le();
            v.set_state(c::bch_subvolume_state::SUBVOLUME_STATE_live);

            let t = t.insert(c::btree_id::subvolumes, k, UpdateTriggerFlags::empty())?;
            (snapshot, inum, t)
        }
    };

    let root_dir_mode = (c::S_IFDIR | 0o755) as c::umode_t;

    match inode::find_by_inum_snapshot(trans, c::BCACHEFS_ROOT_INO as u64, snapshot,
                                       BtreeIterFlags::empty()).found()? {
        None => {
            if mustfix_fsck_err!(trans, id::root_dir_missing, "root directory missing")? {
                let mut root_inode = inode::init(fs, 0, 0, root_dir_mode, 0, None);
                root_inode.bi_inum     = inum;
                root_inode.bi_snapshot = snapshot;

                return bch_err_msg!(fs, inode::fsck_write(t, &mut root_inode),
                                    "writing root inode");
            }
        }
        Some(mut root_inode) => {
            if mustfix_fsck_err_on!(trans, !root_inode.is_dir(),
                                    id::root_inode_not_dir, "root inode not a directory")? {
                // The inode exists: fix only the mode. Reinitializing it
                // generates a fresh hash_seed - invalidating the hash offset of
                // every dirent in the root directory - and wipes bi_subvol:
                root_inode.bi_mode = root_dir_mode;

                return bch_err_msg!(fs, inode::fsck_write(t, &mut root_inode),
                                    "writing root inode");
            }
        }
    }

    Ok(t)
}

/// Get the root directory, creating it if it doesn't exist.
fn check_root(fs: &Fs) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    commit_do(&trans, None, CommitFlags::NO_ENOSPC, check_root_trans)
}

crate::recovery_pass!(bch2_check_root => check_root);
