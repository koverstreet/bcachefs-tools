// SPDX-License-Identifier: GPL-2.0

//! fsck: the check_xattrs recovery pass - every xattr belongs to a live inode
//! in its snapshot, sits at the offset its hash says, and an inode with a
//! POSIX ACL xattr has the matching BCH_INODE_has_*_acl flag.
//!
//! The flag check is one direction only: flag clear with an ACL xattr present
//! breaks the bch2_get_acl() short circuit, hiding the ACL. The other way - a
//! flag with no xattr - costs only a lookup, and check_inode() checks it.
//!
//! An xattr is visible from every snapshot version of its inode descending
//! from the xattr's snapshot that hasn't overwritten it, so every visible
//! version needs the flag. The repairs write attempt-local copies of the
//! walker's inodes: a restarted commit must see the check fire again, and once
//! the repair commits the walker refetches (see InodeWalker).

use crate::btree::bkey::{pos, BkeySC};
use crate::btree::iter::{
    BtreeIter, BtreeIterFlags, CommitFlags, TransAttempt, TransRet,
};
use crate::c;
use crate::check::{InodeWalker, SnapshotsSeen};
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::init::error::id;
use crate::init::progress::Progress;
use crate::snapshots::snapshot;
use crate::xattr::Xattrs;
use crate::{fsck_err_on, inode, str_hash};

struct CheckXattrs {
    s:         SnapshotsSeen,
    inode:     InodeWalker,
    hash_info: c::bch_hash_info,
}

/// The inode flag a POSIX ACL xattr of @x_type needs, with its fsck error and
/// the ACL's name (with article, then bare).
fn acl_flag(x_type: u8)
    -> Option<(c::bch_inode_flags, c::bch_sb_error_id, &'static str, &'static str)>
{
    match x_type as u32 {
        c::KEY_TYPE_XATTR_INDEX_POSIX_ACL_ACCESS =>
            Some((c::bch_inode_flags::BCH_INODE_has_access_acl,
                  id::inode_has_access_acl_flag_wrong, "an access", "access")),
        c::KEY_TYPE_XATTR_INDEX_POSIX_ACL_DEFAULT =>
            Some((c::bch_inode_flags::BCH_INODE_has_default_acl,
                  id::inode_has_default_acl_flag_wrong, "a default", "default")),
        _ => None,
    }
}

fn check_acl_flag<'a, 't>(
    t:      TransAttempt<'a, 't>,
    st:     &mut CheckXattrs,
    k:      BkeySC<'_>,
    x_type: u8,
) -> TransRet<'a, 't> {
    let Some((flag, err, an_acl, acl)) = acl_flag(x_type) else { return Ok(t) };
    let trans = t.trans();
    let fs = trans.fs();
    let flag = flag as u32;

    let mut t = t;
    for i in st.inode.visible_mut(trans, &mut st.s, k.k.p.snapshot) {
        // One repair per visible version, bounded only by snapshot count:
        t = t.commit_lazy_if_full(CommitFlags::NO_ENOSPC)?;

        if !i.whiteout &&
           fsck_err_on!(trans, i.inode.bi_flags & flag == 0, err,
                        "inode has {an_acl} acl xattr but BCH_INODE_has_{acl}_acl not set\n{}\n{}",
                        i.inode, k.to_text(fs))? {
            let mut u = i.inode;
            u.bi_flags |= flag;
            t = inode::fsck_write(t, &mut u)?;
        }
    }

    Ok(t)
}

fn check_xattr<'a, 't>(
    t:    TransAttempt<'a, 't>,
    iter: &mut BtreeIter<'t>,
    k:    BkeySC<'_>,
    st:   &mut CheckXattrs,
) -> TransRet<'a, 't> {
    let trans = t.trans();
    let fs = trans.fs();

    if snapshot::check_key_has_snapshot(trans, iter, k)? {
        return Ok(t);
    }

    st.s.update(k.k.p)?;

    let (t, w) = st.inode.walk(t, iter, k)?;
    let Some(w) = w else { return Ok(t) };
    if w.first_this_inode {
        st.hash_info = str_hash::hash_info_init(fs, w.inode)?;
    }

    let t = match k.as_xattr() {
        Some(x) => check_acl_flag(t, st, k, x.x_type)?,
        None    => t,
    };

    if let Err(e) = str_hash::check_key::<Xattrs>(trans, None, &mut st.hash_info, k, &mut false) {
        // A hash info repair leaves the walker's inodes - and hash_info,
        // initialized from them - stale:
        st.inode.invalidate();
        return Err(e.into());
    }

    Ok(t)
}

fn check_xattrs(fs: &Fs) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    let progress = Progress::recovery(fs, c"bch2_check_xattrs", &[c::btree_id::xattrs], &[]);
    let mut st = CheckXattrs {
        s:         SnapshotsSeen::new(),
        inode:     InodeWalker::new(),
        hash_info: Default::default(),
    };

    let mut iter = BtreeIter::new(&trans, c::btree_id::xattrs,
                                  pos(c::BCACHEFS_ROOT_INO as u64, 0),
                                  BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);

    iter.for_each_commit(&trans, None, CommitFlags::NO_ENOSPC,
        |t, iter, k| {
            let t = progress.update(t, iter)?;
            check_xattr(t, iter, k, &mut st)
        })
}

crate::recovery_pass!(bch2_check_xattrs => check_xattrs);
