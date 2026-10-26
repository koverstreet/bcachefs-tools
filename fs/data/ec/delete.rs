// SPDX-License-Identifier: GPL-2.0

//! Deleting stripes gone empty: the stripe trigger queues the work, and
//! the worker walks the stripe lru's empty end.

use crate::alloc::lru::{LruId, LruPos};
use crate::btree::bkey::{pos, BkeyV};
use crate::btree::iter::{BtreeIter, BtreeIterFlags, CommitFlags, TransAttempt, UpdateTriggerFlags};
use crate::c;
use crate::data::ec::stripe::STRIPE_LRU_POS_EMPTY;
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::sb::counters::events;
use crate::util::ffi::Opaque;
use crate::util::place::Placed;
use crate::{btree_trans, fs_inconsistent, prt};
use cstruct_macros::c_export;

// ── Stripe deletion ──────────────────────────────────────────────────────
//
// A stripe is deleted once it's empty, and not while it's open - held by a
// stripe being created, or by repair. The stripe trigger deletes one that
// empties while it isn't; the holders delete theirs as they let go; and the
// worker deletes whatever empty stripes the stripe lru still has.

/// Delete stripe @idx if it's empty: ec_stripe_delete(). @is_open: the
/// caller holds it open, so it must exist.
pub(crate) fn stripe_delete(t: &TransAttempt<'_, '_>, idx: u64, is_open: bool) -> Result<(), BchError> {
    let fs = t.fs();
    let mut iter = BtreeIter::new(t, c::btree_id::stripes, pos(0, idx), BtreeIterFlags::INTENT);
    let k = iter.peek_slot(t)?.expect("a slot");

    // checked with the stripe intent locked: an existing stripe is opened
    // only under that lock - bch2_stripe_tryget_existing() - so a
    // create can't open it, and load the key we're deleting, before we commit
    if !is_open && fs.stripe_is_open(idx) {
        return Ok(());
    }

    // the worker races with the write buffer: its lru entry can outlive the
    // stripe. An open stripe must exist, but needn't be empty.
    let Some(s) = k.arm::<c::Stripe>() else {
        if is_open {
            fs_inconsistent!(fs, "error deleting stripe: open stripe {idx} missing\n{}", k.to_text(fs));
        }
        return Ok(());
    };

    if s.v().lru_pos() != STRIPE_LRU_POS_EMPTY {
        return Ok(());
    }

    fs.event_inc_trace(events::stripe_delete, |out| prt!(out, "{}", k.to_text(fs)));
    t.delete_at(&iter, UpdateTriggerFlags::empty())
}

/// Deletes empty stripes that aren't open. They sit at lru time
/// STRIPE_LRU_POS_EMPTY, ahead of every reusable one: it walks just those.
///
/// XXX: can we kill this and delete stripes from the trigger?
pub(crate) fn delete_empty_stripes(ec: &c::bch_fs_ec) {
    let fs = ec.container();
    let trans = btree_trans!(fs);
    let at = |offset| LruPos::new(LruId::Stripes.id(), STRIPE_LRU_POS_EMPTY, offset).to_bpos();

    // as C's: errors - going read-only - end the walk, and are dropped
    let _ = trans.write_buffer_tryflush().and_then(|_| {
        BtreeIter::new(&trans, c::btree_id::lru, at(0), BtreeIterFlags::empty())
            .for_each_max_commit(&trans, at(u64::MAX), BtreeIterFlags::empty(), None,
                                 CommitFlags::NO_ENOSPC, |t, _, k| stripe_delete(t, k.k.p.offset, false))
    });
}

impl Fs {
    /// Kick the worker that deletes empty stripes, unless the filesystem's
    /// going read-only: bch2_do_stripe_deletes().
    pub fn do_stripe_deletes(&self) {
        self.ec.stripe_delete_work.queue(self.write_ref_wq());
    }
}

#[c_export]
pub fn bch2_do_stripe_deletes(c: &Opaque<c::bch_fs>) {
    Fs::from_c(c).do_stripe_deletes()
}
