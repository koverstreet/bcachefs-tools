// SPDX-License-Identifier: GPL-2.0

//! The btree node cache, from Rust: so far, pinning - fsck passes keeping the
//! nodes they're about to make random-order lookups in in memory, for as long
//! as a BtreeCachePins lives.

use crate::btree::iter::BtreeTrans;
use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::fs::Fs;
use core::ops::RangeInclusive;

/// What's pinned stays in memory, shrinker-exempt, until this drops. There's one
/// set of pins, the filesystem's: pin() replaces what was.
#[must_use = "everything is unpinned as soon as this drops"]
pub struct BtreeCachePins<'f>(&'f Fs);

impl Fs {
    pub fn btree_cache_pins(&self) -> BtreeCachePins<'_> {
        BtreeCachePins(self)
    }
}

impl BtreeCachePins<'_> {
    /// fsck_memory_usage_percent of ram, in bytes.
    pub fn budget(&self) -> u64 {
        unsafe { c::bch2_btree_cache_pin_budget(self.0.as_ptr()) }
    }

    /// @leaf_mask's btrees to their leaves, @interior_mask's to their interior
    /// nodes, until the budget runs out; nodes read into the window later are
    /// pinned as they come in. Returns where the window ends.
    pub fn pin(&self, trans: &BtreeTrans<'_>, leaf_mask: u64, interior_mask: u64,
               range: RangeInclusive<c::bbpos>) -> Result<c::bbpos, BchError> {
        let (start, end) = range.into_inner();
        let mut pinned_end = end;
        ret_to_result_void(unsafe {
            c::bch2_btree_cache_pin(trans.raw(), leaf_mask, interior_mask, start, end, &mut pinned_end)
        })?;
        Ok(pinned_end)
    }

    /// Outside any window.
    pub fn node(&self, b: &c::btree) {
        unsafe { c::bch2_node_pin(self.0.as_ptr(), core::ptr::from_ref(b).cast_mut()) }
    }

    pub fn unpin(&self) {
        unsafe { c::bch2_btree_cache_unpin(self.0.as_ptr()) }
    }
}

impl Drop for BtreeCachePins<'_> {
    fn drop(&mut self) {
        self.unpin()
    }
}
