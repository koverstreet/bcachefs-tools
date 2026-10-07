// SPDX-License-Identifier: GPL-2.0

//! fsck: the fix_reflink_p recovery pass - an upgrade fixup. On a filesystem
//! older than bcachefs_metadata_version_reflink_p_fix, zero front_pad and
//! back_pad on every reflink pointer, without running triggers.

use crate::btree::bkey::{BkeySC, POS_MIN};
use crate::btree::iter::{
    BtreeIter, BtreeIterFlags, CommitFlags, TransAttempt,
    UpdateTriggerFlags,
};
use crate::c;
use crate::errcode::BchError;
use crate::fs::Fs;

fn fix_reflink_p_key<'t>(
    t:    &TransAttempt<'_, 't>,
    iter: &BtreeIter<'t>,
    k:    BkeySC<'_>,
) -> Result<(), BchError> {
    let Some(p) = k.as_reflink_p() else {
        return Ok(());
    };

    if p.front_pad == 0 && p.back_pad == 0 {
        return Ok(());
    }

    let mut u = t.bkey_reassemble(k)?;
    let v = u.k_i_mut().as_mut_reflink_p().expect("reassembled from a reflink_p");
    v.front_pad = 0.into();
    v.back_pad  = 0.into();

    t.update(iter, &u, UpdateTriggerFlags::NORUN)
}

fn fix_reflink_p(fs: &Fs) -> Result<(), BchError> {
    if fs.version() >= c::bcachefs_metadata_version::reflink_p_fix {
        return Ok(());
    }

    let trans = crate::btree_trans!(fs);
    let mut iter = BtreeIter::new(&trans, c::btree_id::extents, POS_MIN,
        BtreeIterFlags::INTENT | BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);

    iter.for_each_commit(&trans, None, CommitFlags::NO_ENOSPC,
        |t, iter, k| fix_reflink_p_key(t, iter, k))
}

crate::recovery_pass!(bch2_fix_reflink_p => fix_reflink_p);
