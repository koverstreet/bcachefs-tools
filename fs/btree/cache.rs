// SPDX-License-Identifier: GPL-2.0

//! The btree node cache, from Rust: so far, pinning - fsck passes keeping the
//! nodes they're about to make random-order lookups in in memory, for as long
//! as a BtreeCachePins lives.

#![allow(non_camel_case_types)]

use crate::btree::bkey::{BkeySC, BkeyVal};
use crate::btree::iter::BtreeTrans;
use crate::btree::node::{BtreeNode, NodeContents};
use crate::btree::path::BtreePath;
use crate::btree::types::{BtreeCacheLists, BtreeCacheLive};
use crate::locking::ExclusiveGuard;
use crate::util::ffi::Opaque;
use crate::util::sleep::Sleep;
use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::fs::Fs;
use core::mem::offset_of;
use core::ops::RangeInclusive;
use core::ptr::NonNull;
use crate::util::ffi::c_char;
use crate::util::kernel::is_vmalloc_addr;
use crate::util::{Printbuf, ToText};
use core::ffi::{c_int, c_uint, c_ulong, c_void};
use core::sync::atomic::Ordering::Relaxed;
use cstruct_macros::{c_const, c_export, c_extern};
use crate::format::Btree;

c_extern! {
    pub fn bch2_fs_btree_init_rw(c: *mut Fs) -> c_int;
    pub fn bch2_fs_btree_exit(c: *mut Fs);
    pub fn bch2_fs_btree_init_early(c: *mut Fs);
    pub fn bch2_fs_btree_init(c: *mut Fs) -> c_int;
    pub fn bch2_btree_id_str(btree: Btree) -> *const c_char;
    pub fn bch2_btree_id_to_text(out: *mut Printbuf, btree: Btree);
    pub fn bch2_btree_id_level_to_text(out: *mut Printbuf, btree: Btree, level: c_uint);
    pub fn __bch2_btree_pos_to_text(out: *mut Printbuf, c: *mut Fs, btree: Btree, level: c_uint, k: c::bkey_s_c);
    pub fn bch2_btree_node_to_text(out: *mut Printbuf, c: *mut Fs, b: *const BtreeNode);
    pub fn __bch2_btree_node_mem_alloc(c: *mut Fs) -> *mut BtreeNode;
    pub fn bch2_btree_node_data_free(b: *mut BtreeNode);
    pub fn bch2_btree_node_mem_free(c: *mut Fs, b: *mut BtreeNode);
    pub fn bch2_btree_cache_to_text(out: *mut Printbuf, bc: *const c::bch_fs_btree_cache);
    pub fn bch2_btree_cache_pin_budget(c: *mut Fs) -> u64;
    pub fn bch2_node_pin(c: *mut Fs, b: *mut BtreeNode);
    pub fn bch2_btree_node_prefetch(trans: *mut BtreeTrans<'_>, path: *mut BtreePath,
                                    k: *const c::bkey_i, btree: Btree, level: c_uint) -> c_int;
    pub fn bch2_btree_cache_unpin(c: *mut Fs);
    pub fn bch2_btree_cache_pin(trans: *mut BtreeTrans<'_>, leaf_mask: u64, interior_mask: u64,
                                start: c::bbpos, end: c::bbpos, pinned_end: *mut c::bbpos)
                                -> core::ffi::c_int;
    pub static bch2_btree_node_flags: [*const c_char; 0];
    pub fn bch2_btree_bounce_alloc_noprof(c: *mut Fs, size: usize, used_mempool: *mut bool) -> *mut c_void;
    pub fn bch2_btree_bounce_free(c: *mut Fs, size: usize, used_mempool: bool, p: *mut c_void);
    pub fn bch2_btree_node_transition_state_locked(bc: *mut c::bch_fs_btree_cache, b: *mut BtreeNode,
                                                   target: c::btree_node_cache_state) -> c_int;
    pub fn bch2_btree_node_write_done_clean(c: *mut Fs, b: *mut BtreeNode);
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

    pub fn as_bytes(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.p.as_ptr().cast(), self.bytes) }
    }

    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.p.as_ptr().cast(), self.bytes) }
    }

    /// The node's buffer this, and this the node's old one.
    pub fn swap(&mut self, b: &BtreeNode, contents: &mut NodeContents) {
        assert!(self.bytes == b.buf_bytes(), "swapping a {} byte bounce for a {} byte node buffer",
                self.bytes, b.buf_bytes());

        let old = core::mem::replace(&mut contents.data, self.p.as_ptr().cast());
        btree_node_buf_swap_account(self.fs, old.cast(), contents.data.cast());
        self.p = NonNull::new(old.cast()).expect("a node's buffer");
    }
}

unsafe impl Send for Bounce<'_> {}

impl core::ops::Deref for Bounce<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl core::ops::DerefMut for Bounce<'_> {
    fn deref_mut(&mut self) -> &mut [u8] {
        self.as_bytes_mut()
    }
}

// Its buffer, until it's dropped: swap() trades it, but isn't reached
// through the bytes
unsafe impl crate::util::bio::MapBuf for Bounce<'_> {
    fn as_raw(&self) -> (*mut u8, usize) {
        (self.p.as_ptr().cast(), self.bytes)
    }
}

impl Drop for Bounce<'_> {
    fn drop(&mut self) {
        unsafe { bch2_btree_bounce_free(self.fs.as_ptr(), self.bytes, self.mempool, self.p.as_ptr().cast()) }
    }
}

/// A node pointer's identity in the node cache - a v2's seq, a v1's first
/// pointer - read as C reads it, the bytes as they are: btree_ptr_hash_val().
/// 0 for a v1 with no pointer, which C reads past its value for.
pub fn btree_ptr_hash_val<V: ?Sized + BkeyVal>(k: BkeySC<'_, V>) -> u64 {
    let offset = match k.k.key_type() {
        c::bch_bkey_type::KEY_TYPE_btree_ptr    => 0,
        c::bch_bkey_type::KEY_TYPE_btree_ptr_v2 => offset_of!(c::BtreePtrV2, repr.seq),
        _ => return 0,
    };
    k.val_bytes().get(offset..offset + 8)
        .map_or(0, |b| u64::from_ne_bytes(b.try_into().expect("8 bytes")))
}

impl ToText for c::bch_fs_btree_cache {
    fn to_text(&self, out: &mut Printbuf) {
        unsafe { bch2_btree_cache_to_text(out.as_raw(), self) }
    }
}

c_const! {
    pub const BTREE_WRITE_IO_LIMIT: u32 = 64;
}

impl c::bch_fs_btree_cache {
    pub fn update_throttle(&self) {
        let (live, dirty) = (self.lock.nr_live(), self.lock.nr_dirty());
        let throttle = self.nr_in_flight_inner.load(Relaxed) > BTREE_WRITE_IO_LIMIT as isize ||
            (live != 0 && dirty > live * 3 / 4);

        if throttle != self.should_throttle.load(Relaxed) {
            self.should_throttle.store(throttle, Relaxed);
        }
    }

    pub fn transition_state_locked(&self, _lists: &mut ExclusiveGuard<'_, c::mutex_noio, BtreeCacheLists, BtreeCacheLive>,
                                   b: &BtreeNode, target: c::btree_node_cache_state) -> Result<(), BchError> {
        ret_to_result_void(unsafe {
            bch2_btree_node_transition_state_locked(core::ptr::from_ref(self).cast_mut(),
                                                    core::ptr::from_ref(b).cast_mut(), target)
        })
    }

    /// Ask the shrinker to free @nr nodes, as memory pressure would. Not
    /// under bc->lock: the scan takes it.
    pub fn shrink(&self, nr: core::ffi::c_ulong) {
        let shrink = self.lock.lock().live[0].shrink;
        unsafe { crate::util::kernel::shrinker_scan(shrink, nr) }
    }
}

impl BtreeNode {
    /// The hashed (live) cache state derived from b's flag bits. A node
    /// belongs on live[].dirty whenever there's pending work - either
    /// BTREE_NODE_dirty (needs a write) or BTREE_NODE_write_in_flight (one is
    /// outstanding); otherwise it belongs on live[].clean.
    ///
    /// Use this anywhere you need to compute the right cache_state from the
    /// current bits - passing it to transition_state_locked() is a no-op when
    /// the node is already in the right state, so callers don't need "do we
    /// need to transition?" conditionals.
    pub fn live_state(&self) -> c::btree_node_cache_state {
        let f = self.flags.load();
        if f.dirty() || f.write_in_flight() {
            c::btree_node_cache_state::BTREE_NODE_CACHE_DIRTY
        } else {
            c::btree_node_cache_state::BTREE_NODE_CACHE_CLEAN
        }
    }
}

#[c_export]
pub fn btree_node_live_state(b: &BtreeNode) -> c::btree_node_cache_state {
    b.live_state()
}

#[c_export]
pub fn bch2_btree_node_wait_on_flag(b: &BtreeNode, bit: c::btree_flags) {
    let m = 1 << bit as u32;
    let Ok(()) = b.flags.wait_until(Sleep::UNINTERRUPTIBLE.io(), |f| f.into_bits() & m == 0);
}

#[c_export]
pub fn bch2_btree_node_wait_on_flag_timeout(b: &BtreeNode, bit: c::btree_flags, timeout: c_ulong) -> bool {
    let m = 1 << bit as u32;
    b.flags.wait_until_timeout(Sleep::UNINTERRUPTIBLE.io(), timeout, |f| f.into_bits() & m == 0)
}

#[c_export]
pub fn bch2_btree_node_wake_flags(b: &BtreeNode) {
    b.flags.wake()
}

#[c_export]
pub fn bch2_btree_cache_update_throttle(c: &Opaque<Fs>) {
    Fs::from_c(c).btree.cache.update_throttle()
}

impl Fs {
    pub fn btree_node_write_done_clean(&self, b: &BtreeNode) {
        unsafe { bch2_btree_node_write_done_clean(self.as_ptr(), core::ptr::from_ref(b).cast_mut()) }
    }

    /// Bytes of btree nodes cached.
    pub fn btree_cache_size(&self) -> u64 {
        let bc = &self.btree.cache;
        (bc.lock.nr_live() * self.opts.btree_node_size.get() as usize) as u64
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
