// SPDX-License-Identifier: GPL-2.0

//! The btree node cache, from Rust: so far, pinning - fsck passes keeping the
//! nodes they're about to make random-order lookups in in memory, for as long
//! as a BtreeCachePins lives.

#![allow(non_camel_case_types)]

use crate::btree::iter::BtreeTrans;
use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::fs::Fs;
use core::ops::RangeInclusive;
use crate::util::ffi::c_char;
use crate::util::{Printbuf, ToText};
use core::ffi::{c_int, c_uint};
use cstruct_macros::c_extern;

c_extern! {
    pub fn bch2_btree_id_str(btree: c::btree_id) -> *const c_char;
    pub fn bch2_btree_id_to_text(out: *mut c::printbuf, btree: c::btree_id);
    pub fn bch2_btree_id_level_to_text(out: *mut c::printbuf, btree: c::btree_id, level: c_uint);
    pub fn bch2_btree_node_to_text(out: *mut c::printbuf, c: *mut c::bch_fs, b: *const c::btree);
    pub fn bch2_btree_cache_to_text(out: *mut c::printbuf, bc: *const c::bch_fs_btree_cache);
    pub fn bch2_btree_cache_pin_budget(c: *mut c::bch_fs) -> u64;
    pub fn bch2_node_pin(c: *mut c::bch_fs, b: *mut c::btree);
    pub fn bch2_btree_node_prefetch(trans: *mut c::btree_trans, path: *mut c::btree_path,
                                    k: *const c::bkey_i, btree: c::btree_id, level: c_uint) -> c_int;
    pub fn bch2_btree_cache_unpin(c: *mut c::bch_fs);
    pub fn bch2_btree_cache_pin(trans: *mut c::btree_trans, leaf_mask: u64, interior_mask: u64,
                                start: c::bbpos, end: c::bbpos, pinned_end: *mut c::bbpos)
                                -> core::ffi::c_int;
    pub static bch2_btree_node_flags: [*const c_char; 0];
}

impl ToText for c::bch_fs_btree_cache {
    fn to_text(&self, out: &mut Printbuf) {
        unsafe { bch2_btree_cache_to_text(out.as_raw(), self) }
    }
}

impl c::bch_fs_btree_cache {
    /// Ask the shrinker to free @nr nodes, as memory pressure would.
    pub fn shrink(&self, nr: core::ffi::c_ulong) {
        unsafe { crate::util::kernel::shrinker_scan(self.live[0].shrink, nr) }
    }
}

impl c::btree_cache_list {
    pub fn nr(&self) -> usize {
        self.nr_clean + self.nr_dirty
    }
}

impl Fs {
    /// Bytes of btree nodes cached.
    pub fn btree_cache_size(&self) -> u64 {
        let bc = &self.btree.cache;
        ((bc.live[0].nr() + bc.live[1].nr()) * self.opts.btree_node_size.get() as usize) as u64
    }
}

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
