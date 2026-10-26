// SPDX-License-Identifier: GPL-2.0

//! Open stripes, and the buckets of stripes being made.
//!
//! A stripe being made or modified is open: its index is in ec.stripes_open,
//! so that the stripe trigger, the delete worker and stripe reuse leave it
//! alone. A stripe being made's buckets are in ec.stripes_new_buckets, so
//! that reconcile doesn't move their data meanwhile. Both are sets, as all
//! anything asks of either is whether something's in: who holds a stripe open
//! is an OpenStripe's type, not the table's. Opening an existing stripe has to
//! fence the stripe trigger's decision about it, too - by cycling a btree node
//! write lock, in trigger.c's bch2_stripe_tryget_existing() until Rust has
//! btree node locks.

use crate::alloc::buckets::bucket_to_u64;
use crate::btree::iter::{BtreeIter, TransAttempt};
use crate::c;
use crate::errcode::{bch_errcode, ret_to_c, ret_to_result, BchError};
use crate::fs::Fs;
use crate::util::alloc::AllocError;
use crate::util::ffi::Opaque;
use core::ffi::c_int;
use cstruct_macros::c_export;

// ── Open stripes, and the buckets of stripes being made ──────────────────

impl Fs {
    /// Whether a stripe being made has a block in @bucket: for what would
    /// move its data meanwhile - reconcile - to leave it.
    pub fn bucket_has_new_stripe(&self, bucket: c::bpos) -> bool {
        self.ec.stripes_new_buckets.contains(&bucket_to_u64(bucket))
    }

    /// Whether stripe @idx is open - being made, or modified, by whoever
    /// holds it - and so not the stripe trigger's to delete.
    pub fn stripe_is_open(&self, idx: u64) -> bool {
        self.ec.stripes_open.contains(&idx)
    }

    /// Open stripe @idx: false if it's open already.
    fn stripe_open(&self, idx: u64) -> Result<bool, BchError> {
        match self.ec.stripes_open.insert_new(idx) {
            Ok(opened) => Ok(opened.is_ok()),
            Err(AllocError) => self.throw(bch_errcode::BCH_ERR_ENOMEM_stripe_open),
        }
    }
}

#[c_export]
pub fn bch2_stripe_is_open(c: &Opaque<c::bch_fs>, idx: u64) -> bool {
    Fs::from_c(c).stripe_is_open(idx)
}

/// For bch2_stripe_tryget_existing(), in trigger.c until Rust has btree node
/// locks: open stripe @idx - 1, or 0 if it's open already - for the
/// OpenStripe::tryget_existing() that called it to hold.
#[c_export]
pub fn bch2_stripe_tryget(c: &Opaque<c::bch_fs>, idx: u64) -> c_int {
    match Fs::from_c(c).stripe_open(idx) {
        Ok(opened) => opened.into(),
        Err(e) => ret_to_c(Err(e)),
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
    /// bch2_stripe_tryget_existing().
    pub fn tryget_existing(t: &TransAttempt<'_, 'f>, iter: &BtreeIter<'_>) -> Result<Option<Self>, BchError> {
        // SAFETY: @iter in the attempt, and not moved - the key cache entry
        // it locks is another iterator's
        let ret = ret_to_result(unsafe { c::bch2_stripe_tryget_existing(iter.raw()) })?;
        Ok((ret != 0).then(|| OpenStripe { fs: t.fs(), idx: iter.pos().offset }))
    }

    /// Open stripe @idx, one that doesn't exist yet - a slot being allocated
    /// for a new stripe: None if it's open already.
    pub fn tryget(fs: &'f Fs, idx: u64) -> Result<Option<Self>, BchError> {
        Ok(fs.stripe_open(idx)?.then(|| OpenStripe { fs, idx }))
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
