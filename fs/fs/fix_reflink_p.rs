// SPDX-License-Identifier: GPL-2.0

//! fsck: the fix_reflink_p recovery pass - an upgrade fixup. On a filesystem
//! older than MetadataVersion::reflink_p_fix, zero front_pad and
//! back_pad on every reflink pointer, without running triggers.

use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{BkeySC, POS_MIN};
use crate::btree::iter::{
    BtreeIter, BtreeIterFlags, CommitFlags, TransAttempt,
    UpdateTriggerFlags,
};
use crate::c;
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::format::{Btree, MetadataVersion};

fn fix_reflink_p_key<'t>(
    t:    &TransAttempt<'_, 't>,
    iter: &BtreeIter<'t>,
    k:    BkeySC<'_>,
) -> Result<(), BchError> {
    let Some(k) = k.arm::<c::ReflinkP>() else {
        return Ok(());
    };
    let p = k.v();

    if p.front_pad == 0 && p.back_pad == 0 {
        return Ok(());
    }

    let mut u = t.bkey_reassemble(k)?;
    let v = u.v_mut();
    v.front_pad = 0.into();
    v.back_pad  = 0.into();

    t.update(iter, &u, UpdateTriggerFlags::NORUN)
}

fn fix_reflink_p(fs: &Fs) -> Result<(), BchError> {
    if fs.version() >= MetadataVersion::reflink_p_fix {
        return Ok(());
    }

    let trans = crate::btree_trans!(fs);
    let mut iter = BtreeIter::new(&trans, Btree::extents, POS_MIN,
        BtreeIterFlags::INTENT | BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);

    iter.for_each_commit(&trans, None, CommitFlags::NO_ENOSPC,
        |t, iter, k| fix_reflink_p_key(t, iter, k))
}

crate::recovery_pass!(fix_reflink_p => fix_reflink_p);
