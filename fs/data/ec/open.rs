// SPDX-License-Identifier: GPL-2.0

//! Open stripes, and the buckets of stripes being made.
//!
//! A stripe being made or modified is open: its index is in ec.stripes_open,
//! so that the stripe trigger, the delete worker and stripe reuse leave it
//! alone. A stripe being made's buckets are in ec.stripes_new_buckets, so
//! that reconcile doesn't move their data meanwhile. Both are sets, as all
//! anything asks of either is whether something's in: who holds a stripe open
//! is an OpenStripe's type, not the table's. Opening an existing stripe has to
//! fence the stripe trigger's decision about it, too - by cycling the key
//! cache entry's write lock.

use crate::alloc::buckets::bucket_to_u64;
use crate::btree::iter::{BtreeIter, TransAttempt};
use crate::errcode::{bch_errcode, BchError};
use crate::fs::Fs;
use crate::locking::MemallocFlags;
use crate::util::alloc::AllocError;
use crate::format::{Bpos, Btree};

// ── Open stripes, and the buckets of stripes being made ──────────────────

impl Fs {
    /// Whether a stripe being made has a block in @bucket: for what would
    /// move its data meanwhile - reconcile - to leave it.
    pub fn bucket_has_new_stripe(&self, bucket: Bpos) -> bool {
        self.ec.stripes_new_buckets.contains(&bucket_to_u64(bucket))
    }

    /// Whether stripe @idx is open - being made, or modified, by whoever
    /// holds it - and so not the stripe trigger's to delete.
    pub fn stripe_is_open(&self, idx: u64) -> bool {
        self.ec.stripes_open.contains(&idx)
    }
}

/// A stripe we have open: its index in ec.stripes_open - keeping the stripe
/// trigger, the delete worker and stripe reuse off it - until this is
/// dropped. Only opening the stripe makes one, and it isn't Clone: the index
/// it takes out is its own.
pub struct OpenStripe<'f> {
    fs:  &'f Fs,
    idx: u64,
}

impl<'f> OpenStripe<'f> {
    /// Open the stripe at @iter, which must be intent locked - an existing
    /// stripe is only opened under that lock: None if it's open already.
    pub fn tryget_existing(t: &TransAttempt<'_, 'f>, iter: &BtreeIter<'_>) -> Result<Option<Self>, BchError> {
        let (fs, idx) = (t.fs(), iter.pos().offset);
        ebug_assert!(iter.btree() == Btree::stripes, "opening an existing stripe in btree {}", iter.btree());

        if fs.stripe_is_open(idx) {
            return Ok(None);
        }

        /*
         * The stripe trigger and device invalidation act on stripes that
         * aren't open, and their decision can outlive a lock drop: commit
         * relocks and retries without re-running triggers. Cycling a write
         * lock advances the lock sequence, so that relock fails.
         *
         * The lock to cycle is the key cache entry: anything updating this key
         * holds it, and an entry is only freed under its write lock - so if
         * there's no entry, they're already invalidated.
         */
        iter.with_key_cache_write_lock(t, || Self::tryget(fs, idx))?
    }

    /// Open stripe @idx, one that doesn't exist yet - a slot being allocated
    /// for a new stripe: None if it's open already.
    pub fn tryget(fs: &'f Fs, idx: u64) -> Result<Option<Self>, BchError> {
        let opened = {
            let _noreclaim = MemallocFlags::noreclaim();
            fs.ec.stripes_open.insert_new(idx)
        };
        match opened {
            Ok(opened) => Ok(opened.is_ok().then(|| OpenStripe { fs, idx })),
            Err(AllocError) => fs.throw(bch_errcode::BCH_ERR_ENOMEM_stripe_open),
        }
    }

    /// For what holds the filesystem otherwise - a stripe being made - to
    /// put.
    pub fn into_claim(self) -> StripeClaim {
        let idx = self.idx;
        core::mem::forget(self);
        StripeClaim(idx)
    }
}

impl Drop for OpenStripe<'_> {
    fn drop(&mut self) {
        stripe_close(self.fs, self.idx)
    }
}

/// An OpenStripe without its borrow of the filesystem: its holder puts it.
#[must_use]
pub struct StripeClaim(u64);

impl StripeClaim {
    pub fn put(self, fs: &Fs) {
        stripe_close(fs, self.0)
    }
}

fn stripe_close(fs: &Fs, idx: u64) {
    assert!(fs.ec.stripes_open.remove(&idx).is_some(), "stripe {idx} open, but not in the open stripes");
}
