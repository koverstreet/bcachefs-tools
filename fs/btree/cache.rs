// SPDX-License-Identifier: GPL-2.0

//! The btree node cache, from Rust: so far, pinning - fsck passes keeping the
//! nodes they're about to make random-order lookups in in memory, for as long
//! as a BtreeCachePins lives.

#![allow(non_camel_case_types)]

use crate::btree::iter::BtreeTrans;
use crate::btree::node::BtreeNode;
use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::fs::Fs;
use core::ops::RangeInclusive;
use core::ptr::NonNull;
use crate::util::ffi::c_char;
use crate::util::kernel::is_vmalloc_addr;
use crate::util::{Printbuf, ToText};
use core::ffi::{c_int, c_uint, c_void};
use cstruct_macros::{c_export, c_extern};
use crate::format::Btree;

c_extern! {
    pub fn bch2_fs_btree_init_rw(c: *mut Fs) -> c_int;
    pub fn bch2_fs_btree_exit(c: *mut Fs);
    pub fn bch2_fs_btree_init_early(c: *mut Fs);
    pub fn bch2_fs_btree_init(c: *mut Fs) -> c_int;
    pub fn bch2_btree_id_str(btree: Btree) -> *const c_char;
    pub fn bch2_btree_id_to_text(out: *mut Printbuf, btree: Btree);
    pub fn bch2_btree_id_level_to_text(out: *mut Printbuf, btree: Btree, level: c_uint);
    pub fn bch2_btree_node_to_text(out: *mut Printbuf, c: *mut Fs, b: *const BtreeNode);
    pub fn bch2_btree_cache_to_text(out: *mut Printbuf, bc: *const c::bch_fs_btree_cache);
    pub fn bch2_btree_cache_pin_budget(c: *mut Fs) -> u64;
    pub fn bch2_node_pin(c: *mut Fs, b: *mut BtreeNode);
    pub fn bch2_btree_node_prefetch(trans: *mut c::btree_trans, path: *mut c::btree_path,
                                    k: *const c::bkey_i, btree: Btree, level: c_uint) -> c_int;
    pub fn bch2_btree_cache_unpin(c: *mut Fs);
    pub fn bch2_btree_cache_pin(trans: *mut c::btree_trans, leaf_mask: u64, interior_mask: u64,
                                start: c::bbpos, end: c::bbpos, pinned_end: *mut c::bbpos)
                                -> core::ffi::c_int;
    pub static bch2_btree_node_flags: [*const c_char; 0];
    pub fn bch2_btree_bounce_alloc_noprof(c: *mut Fs, size: usize, used_mempool: *mut bool) -> *mut c_void;
    pub fn bch2_btree_bounce_free(c: *mut Fs, size: usize, used_mempool: bool, p: *mut c_void);
}

/// Up to a node's size, to sort a node's keys into: kvmalloc()'d, or the
/// bounce pool's, so it never fails. Freed when dropped - whichever buffer it
/// holds then: swap() trades it for a node's, which it's allocated as.
pub struct Bounce<'f> {
    fs:      &'f Fs,
    p:       NonNull<u64>,
    bytes:   usize,
    mempool: bool,
}

impl<'f> Bounce<'f> {
    pub fn new(fs: &'f Fs, bytes: usize) -> Self {
        let mut mempool = false;
        let p = unsafe { bch2_btree_bounce_alloc_noprof(fs.as_ptr(), bytes, &mut mempool) };

        Self { fs, p: NonNull::new(p.cast()).expect("the bounce pool waits for an element"), bytes, mempool }
    }

    pub fn u64s(&mut self) -> &mut [u64] {
        unsafe { core::slice::from_raw_parts_mut(self.p.as_ptr(), self.bytes / size_of::<u64>()) }
    }

    /// The node's buffer this, and this the node's old one.
    pub fn swap(&mut self, b: &mut BtreeNode) {
        assert!(self.bytes == b.buf_bytes(), "swapping a {} byte bounce for a {} byte node buffer",
                self.bytes, b.buf_bytes());

        let old = core::mem::replace(&mut b.data, self.p.as_ptr().cast());
        self.p = NonNull::new(old.cast()).expect("a node's buffer");
    }

    pub fn as_ptr(&self) -> *const c_void {
        self.p.as_ptr().cast()
    }
}

impl Drop for Bounce<'_> {
    fn drop(&mut self) {
        unsafe { bch2_btree_bounce_free(self.fs.as_ptr(), self.bytes, self.mempool, self.p.as_ptr().cast()) }
    }
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

/// A node's buffer swapped, @old for @new: the cache counts the vmalloc'd.
#[c_export]
pub fn btree_node_buf_swap_account(fs: &Fs, old: *const c_void, new: *const c_void) {
    let delta = is_vmalloc_addr(new) as isize - is_vmalloc_addr(old) as isize;

    if delta != 0 {
        let mut lists = fs.btree.cache.lock.lock();
        lists.nr_vmalloc = lists.nr_vmalloc.wrapping_add_signed(delta);
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
    pub fn node(&self, b: &BtreeNode) {
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
